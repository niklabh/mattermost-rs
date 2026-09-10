//! Cross-server parity for `api4/report.go`'s two reads: `GET /api/v4/reports/users` and
//! `GET /api/v4/reports/users/count`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity user_reports
//! ```
//!
//! # Almost every assertion is scoped by `search_term`
//!
//! The unfiltered report is a page of *the whole users table*, which every other suite in this
//! binary adds to and deactivates rows in. Scoping to this fixture's own username prefix makes
//! the answer depend on nothing any other suite does, which is what lets the tests assert on
//! **which users came back** rather than only on "the two servers agree".
//!
//! # The one shared thing this suite touches
//!
//! [`refresh_post_stats`] refreshes the `PostStats` materialized view. Nothing else in the
//! installation reads it — it exists for this report and for the batch export — and without a
//! refresh every `total_posts` is absent and every `days_active` is `0`, so the whole date-range
//! half of the query is dead and a mutation to it survives.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_direct_channel, create_plain_user, create_team, delete_plain_user,
    fetch_both_raw, fetch_both_stable, go_minted_token, plant_bot, post_message,
    purge_api_fixtures, remove_user_from_team, set_user_roles, stack_enabled,
};

const PATH: &str = "/api/v4/reports/users";
const COUNT_PATH: &str = "/api/v4/reports/users/count";

/// Go's seven sort columns and the JSON key each one lands under, so a test can read the cursor
/// value out of a row it just fetched.
const SORT_COLUMNS: [(&str, &str); 7] = [
    ("CreateAt", "create_at"),
    ("Username", "username"),
    ("FirstName", "first_name"),
    ("LastName", "last_name"),
    ("Nickname", "nickname"),
    ("Email", "email"),
    ("Roles", "roles"),
];

struct Fixture {
    team_id: String,
    /// Active, in the team, and the only fixture user with posts.
    poster_id: String,
    /// Active, in the team, no posts.
    quiet_id: String,
    /// Deactivated — `DeleteAt > 0`.
    dead_id: String,
    /// Removed from every team, so `has_no_team` finds it and nothing else does.
    teamless_id: String,
    /// `system_guest`, in exactly one channel.
    guest_solo_id: String,
    /// `system_guest`, in more than one.
    guest_social_id: String,
    /// A **second** guest in more than one channel, so `single_channel` and `multi_channel` do
    /// not both answer "one row" — where a mutation swapping them is invisible.
    guest_crowd_id: String,
    /// A user whose email is **not** derived from their username.
    ///
    /// `create_plain_user` builds `{username}@mmrs.invalid`, so for every other fixture user any
    /// substring of the username is also a substring of the email and the search query's five
    /// `LIKE` arms can never disagree. This one separates them.
    odd_id: String,
    /// A bot, which the report excludes with a `NOT IN (SELECT UserId FROM Bots)` all of its own.
    bot_username: String,
    /// Whether the channel-count surgery and the view refresh both landed; the guest and
    /// date-range assertions are skipped without a database.
    planted: bool,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "urep").await;
            let channel_id = create_channel_typed(client, token, &team_id, "urep", "O").await;

            let poster = create_plain_user(client, token, &team_id, "urepposter").await;
            add_user_to_channel(client, token, &channel_id, &poster.id).await;
            for n in 0..3 {
                post_message(
                    client,
                    &poster.token,
                    &channel_id,
                    &format!("urep {n}"),
                    None,
                )
                .await;
            }

            let quiet = create_plain_user(client, token, &team_id, "urepquiet").await;

            let dead = create_plain_user(client, token, &team_id, "urepdead").await;
            delete_plain_user(client, token, &dead.id).await;

            let teamless = create_plain_user(client, token, &team_id, "urepnoteam").await;
            remove_user_from_team(client, token, &team_id, &teamless.id).await;

            let guest_solo = create_plain_user(client, token, &team_id, "urepguestone").await;
            let guest_social = create_plain_user(client, token, &team_id, "urepguesttwo").await;
            let guest_crowd = create_plain_user(client, token, &team_id, "urepguestthree").await;
            add_user_to_channel(client, token, &channel_id, &guest_social.id).await;
            add_user_to_channel(client, token, &channel_id, &guest_crowd.id).await;

            let odd = create_plain_user(client, token, &team_id, "urepodd").await;

            let bot_username = "mmrsbotparityurepbot".to_owned();
            let bot = plant_bot("urepbot", common::logged_in_user_id(), 0).await;

            let mut planted = bot.is_some()
                && set_user_roles(&guest_solo.id, "system_guest").await
                && set_user_roles(&guest_social.id, "system_guest").await
                && set_user_roles(&guest_crowd.id, "system_guest").await
                && set_email(&odd.id, ODD_EMAIL).await
                && keep_one_channel(&guest_solo.id).await;

            // **After** the surgery, never before: `keep_one_channel` deletes every membership
            // row but one, and a direct-message membership is a row like any other.
            if planted {
                create_direct_channel(client, token, &guest_solo.id, &poster.id).await;
            }

            planted = planted
                && plant_dated_post(&channel_id, &poster.id, previous_month_millis()).await
                && plant_dated_post(&channel_id, &poster.id, this_month_millis()).await
                && plant_dated_post(&channel_id, &poster.id, hundred_days_ago_millis()).await
                && refresh_post_stats().await;

            Fixture {
                team_id,
                poster_id: poster.id,
                quiet_id: quiet.id,
                dead_id: dead.id,
                teamless_id: teamless.id,
                guest_solo_id: guest_solo.id,
                guest_social_id: guest_social.id,
                guest_crowd_id: guest_crowd.id,
                odd_id: odd.id,
                bot_username,
                planted,
                plain_token: quiet.token,
            }
        })
        .await
}

async fn fixture_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()
}

/// The email planted on the `urepodd` user — deliberately sharing no substring with their
/// username, so a term that matches one cannot match the other.
const ODD_EMAIL: &str = "quirkystranger@mmrs.invalid";

/// Replace a user's email.
///
/// `POST /api/v4/users` derives the email from the username and every fixture user therefore has
/// `{username}@mmrs.invalid`. That makes `generateSearchQuery`'s `Username` and `Email` arms
/// indistinguishable: any term matching one matches the other, and a mutation that breaks either
/// arm alone survives. One user with an unrelated email separates them.
async fn set_email(user_id: &str, email: &str) -> bool {
    let Some(pool) = fixture_pool().await else {
        return false;
    };
    sqlx::query("UPDATE users SET email = $2 WHERE id = $1")
        .bind(user_id)
        .bind(email)
        .execute(&pool)
        .await
        .is_ok()
}

/// Cut a user down to **exactly one** open-or-private channel.
///
/// Joining a team auto-joins `town-square` *and* `off-topic`, and Go refuses to remove anyone
/// from a default channel, so one membership is not arrangeable through the API. The guest filter
/// counts `= 1` against `> 1`, and without a user on each side of that line the two branches are
/// indistinguishable.
async fn keep_one_channel(user_id: &str) -> bool {
    let Some(pool) = fixture_pool().await else {
        return false;
    };
    sqlx::query(
        "DELETE FROM channelmembers cm
               USING channels c
               WHERE c.id = cm.channelid
                 AND cm.userid = $1
                 AND c.id <> (SELECT c2.id
                                FROM channelmembers cm2
                                JOIN channels c2 ON c2.id = cm2.channelid
                               WHERE cm2.userid = $1
                                 AND c2.deleteat = 0
                                 AND c2.type IN ('O', 'P')
                               ORDER BY c2.id
                               LIMIT 1)",
    )
    .bind(user_id)
    .execute(&pool)
    .await
    .is_ok()
}

/// Noon on the **first of the previous month**, local time.
///
/// The date-range window `previous_month` is `[first of last month, first of this month)`, and
/// this instant is the only one in the fixture inside it: today's posts are past its end, and the
/// hundred-day-old post is before its start. So a single row proves both bounds at once.
fn previous_month_millis() -> i64 {
    use chrono::{Datelike, TimeZone};
    let now = chrono::Local::now();
    let (year, month) = if now.month() == 1 {
        (now.year() - 1, 12)
    } else {
        (now.year(), now.month() - 1)
    };
    chrono::Local
        .with_ymd_and_hms(year, month, 1, 12, 0, 0)
        .single()
        .expect("noon on the first of a month exists")
        .timestamp_millis()
}

/// Noon on the **first of this month** — the day `previous_month`'s *end* bound falls on.
///
/// `ps.Day < endDate` and `ps.Day <= endDate` differ on exactly this date and nowhere else, so
/// without a row here the end bound's strictness is untestable and a mutation loosening it
/// survives.
fn this_month_millis() -> i64 {
    use chrono::{Datelike, TimeZone};
    let now = chrono::Local::now();
    chrono::Local
        .with_ymd_and_hms(now.year(), now.month(), 1, 12, 0, 0)
        .single()
        .expect("noon on the first of a month exists")
        .timestamp_millis()
}

/// The distinct local dates the fixture's posts fall on.
///
/// Computed rather than written down as `4`: on the first of a month, "today" and
/// [`this_month_millis`] are the same day, and the report would answer three.
fn distinct_post_days() -> i64 {
    use std::collections::BTreeSet;
    let day = |millis: i64| {
        chrono::DateTime::from_timestamp_millis(millis)
            .expect("a representable instant")
            .with_timezone(&chrono::Local)
            .date_naive()
    };
    BTreeSet::from([
        day(chrono::Local::now().timestamp_millis()),
        day(this_month_millis()),
        day(previous_month_millis()),
        day(hundred_days_ago_millis()),
    ])
    .len() as i64
}

/// A hundred days ago — always outside `last_30_days` and always inside `last_6_months`, on any
/// day of any year, which is what makes the assertions about those two ranges date-independent.
fn hundred_days_ago_millis() -> i64 {
    (chrono::Local::now() - chrono::Duration::days(100)).timestamp_millis()
}

/// A post at a chosen instant.
///
/// `POST /api/v4/posts` stamps `CreateAt` itself, and `PostStats` groups by the day that column
/// falls on — so a fixture that only posts through the API has every row on today's date and the
/// join condition the date range exists for is never exercised.
async fn plant_dated_post(channel_id: &str, user_id: &str, created_at: i64) -> bool {
    let Some(pool) = fixture_pool().await else {
        return false;
    };
    sqlx::query(
        "INSERT INTO posts (id, createat, updateat, deleteat, userid, channelid, rootid,
                            originalid, message, type, props, hashtags, filenames, fileids,
                            hasreactions, editat, ispinned, remoteid)
         VALUES ($1, $2, $2, 0, $3, $4, '', '', 'dated by the user_reports suite', '',
                 '{}'::jsonb, '', '[]', '[]', false, 0, false, NULL)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(format!(
        "mmrsurepdated{:013}",
        created_at % 10_000_000_000_000
    ))
    .bind(created_at)
    .bind(user_id)
    .bind(channel_id)
    .execute(&pool)
    .await
    .is_ok()
}

/// `SqlUserStore.RefreshPostStatsForUsers` (user_store.go:2438), which a scheduled job calls and
/// no route does.
async fn refresh_post_stats() -> bool {
    let Some(pool) = fixture_pool().await else {
        return false;
    };
    sqlx::query("REFRESH MATERIALIZED VIEW poststats")
        .execute(&pool)
        .await
        .is_ok()
}

/// One query against both servers, asserted byte-identical, parsed.
///
/// Through `fetch_both_stable` rather than `fetch_both`: `last_status_at` is `MAX(Status
/// .LastActivityAt)` and every request a fixture user makes moves it, this suite's own 403 test
/// included.
async fn both(client: &reqwest::Client, token: &str, query: &str) -> serde_json::Value {
    let path = format!("{PATH}?{query}");
    let (go, rs) = fetch_both_stable(client, token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "`json.NewEncoder(w).Encode` appends a newline"
    );
    serde_json::from_slice(&go).expect("JSON")
}

/// The same, for the count route, which answers a bare number.
async fn both_count(client: &reqwest::Client, token: &str, query: &str) -> i64 {
    let path = if query.is_empty() {
        COUNT_PATH.to_owned()
    } else {
        format!("{COUNT_PATH}?{query}")
    };
    let (go, rs) = fetch_both_stable(client, token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert_eq!(go.last(), Some(&b'\n'), "the encoder's newline");
    serde_json::from_slice::<i64>(&go).expect("a bare number")
}

fn ids(rows: &serde_json::Value) -> Vec<String> {
    rows.as_array()
        .expect("an array")
        .iter()
        .map(|row| row["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// Every fixture user, in whatever order the sort asks for.
const SCOPED: &str = "page_size=100&search_term=mmrsplainurep";

/// How many users [`fixture`] builds. Every scoped page is exactly this long.
const FIXTURE_USERS: i64 = 8;

#[tokio::test]
async fn the_scoped_page_is_this_fixture_and_matches_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let rows = both(&client, &token, SCOPED).await;
    let found = ids(&rows);

    for (label, id) in [
        ("poster", &f.poster_id),
        ("quiet", &f.quiet_id),
        ("deactivated", &f.dead_id),
        ("teamless", &f.teamless_id),
        ("guest_solo", &f.guest_solo_id),
        ("guest_social", &f.guest_social_id),
        ("guest_crowd", &f.guest_crowd_id),
        ("odd", &f.odd_id),
    ] {
        assert!(found.contains(id), "{label} is in the unfiltered report");
    }
    assert_eq!(
        found.len(),
        FIXTURE_USERS as usize,
        "and nobody else is: {found:?}"
    );

    // `Teams` is `string_agg` over the user's *undeleted* team memberships, and the teamless user
    // left theirs — a soft delete the aggregate's own `tm.DeleteAt = 0` is what excludes.
    let teamless = rows
        .as_array()
        .expect("an array")
        .iter()
        .find(|row| row["id"] == f.teamless_id.as_str())
        .expect("the teamless user");
    assert!(
        teamless.get("teams").is_none(),
        "an empty `Teams` is `omitempty`, so the key is absent: {teamless}"
    );
}

/// An empty page is `[]`, not `null` — `make([]*model.UserReport, 0)` is non-nil.
#[tokio::test]
async fn an_empty_report_is_an_empty_array() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let (go, rs) = fetch_both_stable(
        &client,
        &token,
        &format!("{PATH}?page_size=10&search_term=mmrsnobodyatallurep"),
    )
    .await;
    assert_eq!(go, rs, "the empty page must be byte-identical");
    assert_eq!(
        String::from_utf8_lossy(&go),
        "[]\n",
        "a non-nil empty slice, not `null`"
    );
}

/// All seven sort columns, both directions.
#[tokio::test]
async fn every_sort_column_and_direction_matches() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for (column, _) in SORT_COLUMNS {
        for direction in ["asc", "desc"] {
            let rows = both(
                &client,
                &token,
                &format!("{SCOPED}&sort_column={column}&sort_direction={direction}"),
            )
            .await;
            assert_eq!(
                ids(&rows).len(),
                FIXTURE_USERS as usize,
                "{column} {direction} pages the fixture"
            );
        }
    }

    // The *sort* actually happens: ascending and descending by username are reverses.
    let up = ids(&both(&client, &token, &format!("{SCOPED}&sort_column=Username")).await);
    let down = ids(&both(
        &client,
        &token,
        &format!("{SCOPED}&sort_column=Username&sort_direction=desc"),
    )
    .await);
    let mut reversed = down.clone();
    reversed.reverse();
    assert_eq!(up, reversed, "`sort_direction=desc` reverses the page");

    // And only the literal `desc` reverses it: Go compares the string, it does not parse it.
    let sideways = ids(&both(
        &client,
        &token,
        &format!("{SCOPED}&sort_column=Username&sort_direction=DESC"),
    )
    .await);
    assert_eq!(sideways, up, "`sort_direction=DESC` is not `desc`");
}

/// The keyset cursor, walked forwards and backwards on every sort column.
#[tokio::test]
async fn the_cursor_walks_the_same_pages_in_both_directions() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    for (column, key) in SORT_COLUMNS {
        for sort_direction in ["asc", "desc"] {
            let head = format!(
                "page_size=2&search_term=mmrsplainurep&sort_column={column}\
                 &sort_direction={sort_direction}"
            );
            let first = both(&client, &token, &head).await;
            let rows = first.as_array().expect("an array");
            assert_eq!(rows.len(), 2, "{head} fills a page of two");

            for (direction, anchor) in [("next", &rows[1]), ("prev", &rows[0])] {
                let value = match anchor[key].as_str() {
                    Some(text) => text.to_owned(),
                    None => anchor[key].to_string(),
                };
                let anchor_id = anchor["id"].as_str().expect("an id").to_owned();
                let page = both(
                    &client,
                    &token,
                    &format!(
                        "{head}&direction={direction}&from_id={anchor_id}&from_column_value={}",
                        urlencoding(&value)
                    ),
                )
                .await;
                if value.is_empty() {
                    // **`FirstName`, `LastName` and `Nickname` are empty on every fixture user**,
                    // and Go's cursor needs `FromId != "" && FromColumnValue != ""` — so an empty
                    // sort value is not a cursor at all and the page comes back unfiltered.
                    assert_eq!(
                        ids(&page),
                        ids(&first),
                        "an empty `from_column_value` is no cursor: {column} {sort_direction}"
                    );
                } else if direction == "next" {
                    assert!(
                        !ids(&page).contains(&anchor_id),
                        "the anchor row is strictly excluded: {column} {sort_direction}"
                    );
                }
            }
        }
    }
}

/// `direction=prev` re-sorts the page it fetched, **with no cursor at all**.
///
/// Go applies the reversing wrapper on `Direction == "prev"` alone, so a first page asked for
/// backwards comes back as the *end* of the ascending order, in ascending order. Both halves
/// matter: a port that skipped the wrapper would return the same rows reversed, and one that
/// applied it to the wrong direction would return the wrong rows.
#[tokio::test]
async fn a_prev_page_with_no_cursor_is_the_tail_in_forward_order() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let head = "page_size=2&search_term=mmrsplainurep&sort_column=Username";
    let all = ids(&both(&client, &token, &format!("{SCOPED}&sort_column=Username")).await);
    let forwards = ids(&both(&client, &token, head).await);
    let backwards = ids(&both(&client, &token, &format!("{head}&direction=prev")).await);

    assert_eq!(forwards, all[..2], "`next` is the head of the order");
    let mut reversed = forwards.clone();
    reversed.reverse();
    assert_eq!(
        backwards, reversed,
        "with no cursor the inner query still sorts ascending, so `prev` fetches the **same** \
         head and the wrapper hands it back reversed — not the tail, which is what a reader \
         expecting `prev` to mean `the page before` would predict"
    );
}

/// The six filters, each one narrowing to the users the fixture built for it.
#[tokio::test]
async fn each_filter_selects_the_users_it_names() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let scoped = |extra: &str| format!("{SCOPED}&{extra}");

    let in_team = ids(&both(
        &client,
        &token,
        &scoped(&format!("team_filter={}", f.team_id)),
    )
    .await);
    assert!(
        in_team.contains(&f.poster_id) && !in_team.contains(&f.teamless_id),
        "`team_filter` joins TeamMembers with `tm.DeleteAt = 0`: {in_team:?}"
    );

    let teamless = ids(&both(&client, &token, &scoped("has_no_team=true")).await);
    assert_eq!(
        teamless,
        vec![f.teamless_id.clone()],
        "`has_no_team` is the only fixture user outside every team"
    );

    // `has_no_team` is an `if`/`else if`: a team id given alongside it is discarded, not ANDed.
    assert_eq!(
        ids(&both(
            &client,
            &token,
            &scoped(&format!("has_no_team=true&team_filter={}", f.team_id))
        )
        .await),
        teamless,
        "`has_no_team` wins over `team_filter` outright"
    );

    let hidden_active = ids(&both(&client, &token, &scoped("hide_active=true")).await);
    assert_eq!(
        hidden_active,
        vec![f.dead_id.clone()],
        "`hide_active` keeps `DeleteAt > 0`, which is the deactivated user alone"
    );

    let hidden_inactive = ids(&both(&client, &token, &scoped("hide_inactive=true")).await);
    assert!(
        !hidden_inactive.contains(&f.dead_id) && hidden_inactive.contains(&f.poster_id),
        "`hide_inactive` keeps `DeleteAt = 0`: {hidden_inactive:?}"
    );

    // The three booleans are string comparisons against `"true"`, not `strconv.ParseBool`.
    assert_eq!(
        ids(&both(&client, &token, &scoped("hide_active=1")).await).len(),
        FIXTURE_USERS as usize,
        "`hide_active=1` is false — Go compares the literal string"
    );

    // `role_filter` is a `LIKE '%…%'` over `Users.Roles`, so a substring matches.
    let admins = ids(&both(&client, &token, &scoped("role_filter=system_admin")).await);
    assert!(admins.is_empty(), "no fixture user is an admin: {admins:?}");
    let users = ids(&both(&client, &token, &scoped("role_filter=system_user")).await);
    assert!(
        users.contains(&f.poster_id) && !users.contains(&f.guest_solo_id),
        "the guests' roles were replaced, not appended to: {users:?}"
    );

    // `search_term` is the scope every other assertion here leans on; check it narrows at all.
    let one = ids(&both(
        &client,
        &token,
        "page_size=100&search_term=mmrsplainurepquiet",
    )
    .await);
    assert_eq!(one, vec![f.quiet_id.clone()], "a term narrows to one user");

    // The pattern is `%term%`, wildcarded on **both** sides — a term that appears only in the
    // middle of the username still matches, which a prefix-only `term%` would miss.
    assert_eq!(
        ids(&both(&client, &token, "page_size=100&search_term=urepquiet").await),
        vec![f.quiet_id.clone()],
        "the search wildcard leads as well as trails"
    );

    // **The `Username` arm alone.** `plainurepodd` is a middle substring of that user's username
    // and appears nowhere in their planted email, so this row can only have come back through
    // `lower(Username) LIKE '%term%'`. Every other fixture user's email contains their username,
    // which makes the two arms indistinguishable and a mutation to either one invisible.
    assert_eq!(
        ids(&both(&client, &token, "page_size=100&search_term=plainurepodd").await),
        vec![f.odd_id.clone()],
        "the username arm, with the email arm unable to help"
    );

    // **And the `Email` arm alone**, through the same user from the other side.
    assert_eq!(
        ids(&both(&client, &token, "page_size=100&search_term=uirkystrange").await),
        vec![f.odd_id.clone()],
        "the email arm, with the username arm unable to help"
    );

    // `generateSearchQuery` adds an `Id = ?` arm beside the five `LIKE`s, so an id is a term.
    assert_eq!(
        ids(&both(
            &client,
            &token,
            &format!("page_size=100&search_term={}", f.poster_id)
        )
        .await),
        vec![f.poster_id.clone()],
        "a 26-character id matches the `Id = ?` arm, which no `LIKE` on a name would"
    );

    // And a leading `@` is trimmed off every term before it is used.
    assert_eq!(
        ids(&both(
            &client,
            &token,
            "page_size=100&search_term=%40mmrsplainurepquiet"
        )
        .await),
        vec![f.quiet_id.clone()],
        "`strings.TrimLeft(term, \"@\")`"
    );
}

/// The three guest filters, which replace the role filter and add a channel-count predicate.
#[tokio::test]
async fn the_guest_filters_split_on_the_channel_count() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let scoped = |extra: &str| format!("{SCOPED}&{extra}");

    let all = ids(&both(&client, &token, &scoped("guest_filter=all")).await);
    let single = ids(&both(&client, &token, &scoped("guest_filter=single_channel")).await);
    let multi = ids(&both(&client, &token, &scoped("guest_filter=multi_channel")).await);

    if !f.planted {
        return;
    }

    assert_eq!(
        all.len(),
        3,
        "`all` is every `system_guest`, whatever their channels: {all:?}"
    );
    assert_eq!(
        single,
        vec![f.guest_solo_id.clone()],
        "`single_channel` counts `= 1` open-or-private channel"
    );
    // **Two** rows on this side and one on the other. With one each, `= 1` and `> 1` return the
    // same *number* of users, and a mutation swapping them is invisible to a count.
    assert_eq!(multi.len(), 2, "`multi_channel` counts `> 1`: {multi:?}");
    assert!(
        multi.contains(&f.guest_social_id) && multi.contains(&f.guest_crowd_id),
        "both social guests: {multi:?}"
    );

    // A guest filter **discards** `role_filter` rather than combining with it: Go reaches
    // `applyRoleFilter(query, filter.Role)` only in the `default` arm of the switch.
    assert_eq!(
        ids(&both(
            &client,
            &token,
            &scoped("guest_filter=all&role_filter=system_admin")
        )
        .await),
        all,
        "`role_filter` is unreachable once a guest filter is set"
    );
}

/// `date_range` moves the aggregates and never the rows.
#[tokio::test]
async fn the_date_range_narrows_the_post_stats_only() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let stats = |rows: &serde_json::Value, id: &str| -> (Option<i64>, Option<i64>) {
        let row = rows
            .as_array()
            .expect("an array")
            .iter()
            .find(|row| row["id"] == id)
            .expect("the user is in the page")
            .clone();
        (
            row.get("total_posts").and_then(serde_json::Value::as_i64),
            row.get("days_active").and_then(serde_json::Value::as_i64),
        )
    };

    let all_time = both(&client, &token, &format!("{SCOPED}&date_range=all_time")).await;
    let recent = both(
        &client,
        &token,
        &format!("{SCOPED}&date_range=last_30_days"),
    )
    .await;
    let previous = both(
        &client,
        &token,
        &format!("{SCOPED}&date_range=previous_month"),
    )
    .await;
    let half_year = both(
        &client,
        &token,
        &format!("{SCOPED}&date_range=last_6_months"),
    )
    .await;

    for (label, rows) in [
        ("last_30_days", &recent),
        ("previous_month", &previous),
        ("last_6_months", &half_year),
    ] {
        assert_eq!(
            ids(&all_time),
            ids(rows),
            "`{label}` reaches the `PostStats` join condition only — it never drops a row"
        );
    }

    if !f.planted {
        return;
    }

    // Three messages posted through the API today and three planted — on the first of this month,
    // the first of last month, and a hundred days back — plus whatever system posts Go wrote in
    // this user's name. So the day *count* is exact and the post total is a floor.
    let all_days = distinct_post_days();
    let (posts, days) = stats(&all_time, &f.poster_id);
    assert!(
        posts >= Some(6),
        "at least the six fixture posts, through the refreshed view: {posts:?}"
    );
    assert_eq!(days, Some(all_days), "spread over {all_days} distinct days");

    // **The start bound.** A hundred days is outside thirty on any date, so this is smaller than
    // the unbounded total whatever day the suite runs on.
    let (recent_posts, recent_days) = stats(&recent, &f.poster_id);
    assert!(
        recent_posts < posts && recent_days < Some(all_days),
        "`last_30_days` drops the hundred-day-old post: {recent_posts:?}/{recent_days:?}"
    );

    // **Both bounds at once, and both are strict in the right direction.** `previous_month` is
    // `[first of last month, first of this month)`: the post planted *on* the start date is
    // inside it, the one planted *on* the end date is not, and the hundred-day-old one is before
    // the start. So `>=` loosened to `>`, or `<` loosened to `<=`, each move this number.
    assert_eq!(
        stats(&previous, &f.poster_id),
        (Some(1), Some(1)),
        "one planted post, on one day, inside the previous month"
    );

    // Six months reaches every planted post, so it is the unbounded answer again.
    assert_eq!(
        stats(&half_year, &f.poster_id),
        (posts, Some(all_days)),
        "`last_6_months` covers everything this fixture planted"
    );

    // The aggregates are **per user**, not per page: the quiet user's only posts are the system
    // messages Go wrote when they joined, so their `total_posts` is real, smaller, and its own.
    // A `SUM` that had lost its grouping would give both users the same number.
    let (quiet_posts, _) = stats(&all_time, &f.quiet_id);
    assert!(
        quiet_posts.is_some() && quiet_posts < posts,
        "the quiet user posted fewer than the poster: {quiet_posts:?} vs {posts:?}"
    );

    // A `SUM` over an empty join is NULL and `omitempty` drops the key; the `COUNT` beside it is
    // zero and keeps it. The two aggregates read the same rows and answer differently.
    assert_eq!(
        stats(&previous, &f.quiet_id),
        (None, Some(0)),
        "`total_posts` absent, `days_active` present-and-zero"
    );
}

/// The count route reads six of the thirteen query parameters and ignores the rest.
#[tokio::test]
async fn the_count_ignores_pagination_sorting_and_the_date_range() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let scoped = both_count(&client, &token, "search_term=mmrsplainurep").await;
    assert_eq!(scoped, FIXTURE_USERS, "every fixture user, counted");

    for ignored in [
        "page_size=1",
        "page_size=0",
        "page_size=99999",
        "sort_column=NotAColumnAtAll",
        "sort_direction=desc",
        "direction=prev",
        "date_range=previous_month",
        "from_id=abc&from_column_value=def",
    ] {
        assert_eq!(
            both_count(
                &client,
                &token,
                &format!("search_term=mmrsplainurep&{ignored}")
            )
            .await,
            scoped,
            "`{ignored}` is never read by getUserCountForReporting — it calls \
             fillUserReportOptions and not fillReportingBaseOptions"
        );
    }

    // The filters it *does* read agree with the list route, row for row.
    for filter in [
        "hide_active=true",
        "hide_inactive=true",
        "has_no_team=true",
        "role_filter=system_guest",
        "guest_filter=single_channel",
        &format!("team_filter={}", f.team_id),
    ] {
        let listed = ids(&both(&client, &token, &format!("{SCOPED}&{filter}")).await).len() as i64;
        let counted = both_count(
            &client,
            &token,
            &format!("search_term=mmrsplainurep&{filter}"),
        )
        .await;
        assert_eq!(counted, listed, "`{filter}` counts what it lists");
    }
}

/// Both routes' 400s, and the order Go checks them in.
#[tokio::test]
async fn the_parameter_errors_match_and_keep_their_order() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let cases: [(&str, &str, u16); 5] = [
        (
            "team_filter=not-an-id",
            "api.getUsersForReporting.invalid_team_filter",
            400,
        ),
        (
            "hide_active=true&hide_inactive=true",
            "api.getUsersForReporting.invalid_active_filter",
            400,
        ),
        (
            "page_size=0",
            "api.getUsersForReporting.invalid_page_size",
            400,
        ),
        (
            "page_size=101",
            "api.getUsersForReporting.invalid_page_size",
            400,
        ),
        (
            "sort_column=NotAColumnAtAll",
            "model.user_report_options.is_valid.invalid_sort_column",
            400,
        ),
    ];

    for (query, id, status) in cases {
        let path = format!("{PATH}?{query}");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, status, "{path}");
        assert_eq!(rs_status, go_status, "{path}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], id, "{path}");
    }

    // `guest_filter` is validated by the model, so its id is the model's — and only a *non-empty*
    // unknown value is refused, which is why `guest_filter=` is a 200.
    let path = format!("{PATH}?page_size=10&guest_filter=some_channel");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!((go_status, rs_status), (400, 400));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(
        go["id"],
        "model.user_report_options.is_valid.invalid_guest_filter"
    );

    // **Order.** `fillUserReportOptions` runs before the page-size bound, so a request that is
    // wrong in both ways reports the filter error. Moving the bound check up is a one-line
    // mutation that nothing else here would catch.
    let path = format!("{PATH}?page_size=0&team_filter=not-an-id");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!((go_status, rs_status), (400, 400));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(
        go["id"], "api.getUsersForReporting.invalid_team_filter",
        "the filter parameters are read before the page size is bounded"
    );

    // And the sort column is the *model's* check, which runs in the app after the handler's
    // bound — so a bad column with a bad page size reports the page size.
    let path = format!("{PATH}?page_size=0&sort_column=NotAColumnAtAll");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!((go_status, rs_status), (400, 400));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(
        go["id"], "api.getUsersForReporting.invalid_page_size",
        "the handler's bound is checked before the model validates the options"
    );

    // The count route reads the same two filter parameters and reports them under the **list**
    // route's `where`, which is Go's copy-paste and is on the wire.
    let path = format!("{COUNT_PATH}?team_filter=not-an-id");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!((go_status, rs_status), (400, 400));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "api.getUsersForReporting.invalid_team_filter");
}

/// Unparseable pagination is silently defaulted, never a 400.
#[tokio::test]
async fn garbage_pagination_falls_back_to_the_defaults() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    // `strconv.ParseInt` fails and the error is discarded, leaving 50 — which is inside the
    // bound, so this is a 200 and not the `invalid_page_size` a stricter reading would give.
    let rows = both(&client, &token, "page_size=lots&search_term=mmrsplainurep").await;
    assert_eq!(
        ids(&rows).len(),
        FIXTURE_USERS as usize,
        "the default page size is 50"
    );

    // Anything that is not exactly `prev` is `next`.
    assert_eq!(
        ids(&both(&client, &token, &format!("{SCOPED}&direction=backwards")).await),
        ids(&both(&client, &token, &format!("{SCOPED}&direction=next")).await),
        "`direction` is a two-value comparison, not a parse"
    );

    // An unknown `date_range` is `GetReportDateRange`'s fall-through: both bounds stay zero.
    assert_eq!(
        ids(&both(
            &client,
            &token,
            &format!("{SCOPED}&date_range=since_tuesday")
        )
        .await),
        ids(&both(&client, &token, &format!("{SCOPED}&date_range=all_time")).await),
        "an unrecognised range is unbounded"
    );

    // A cursor is applied only when **both** halves are present.
    let head = format!("{SCOPED}&sort_column=Username");
    let full = ids(&both(&client, &token, &head).await);
    assert_eq!(
        ids(&both(
            &client,
            &token,
            &format!("{head}&from_id=abcdefghijklmnopqrstuvwxyz")
        )
        .await),
        full,
        "`from_id` alone is not a cursor"
    );
    assert_eq!(
        ids(&both(
            &client,
            &token,
            &format!("{head}&from_column_value=mmrsplainurep")
        )
        .await),
        full,
        "`from_column_value` alone is not a cursor either"
    );
}

/// A `CreateAt` cursor whose value is not a number is a **500** on both servers.
///
/// Go binds `FromColumnValue` as a string and lets Postgres coerce it against a `bigint` column,
/// so the query fails and `GetUsersForReporting` wraps it into `app.report.get_user_report
/// .store_error`. A port that compared against SQL NULL instead would answer 200 with an empty
/// page, which is the plausible wrong answer this test exists to refuse.
#[tokio::test]
async fn a_non_numeric_cursor_on_create_at_fails_the_query() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = format!(
        "{PATH}?page_size=10&sort_column=CreateAt\
         &from_id=abcdefghijklmnopqrstuvwxyz&from_column_value=yesterday"
    );
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 500, "{path}");
    assert_eq!(rs_status, go_status, "{path}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "app.report.get_user_report.store_error");

    // The same cursor on a text column is fine — nothing is coerced.
    let ok = format!(
        "{SCOPED}&sort_column=Username\
         &from_id=abcdefghijklmnopqrstuvwxyz&from_column_value=yesterday"
    );
    both(&client, &token, &ok).await;
}

/// `sysconsole_read_user_management_users`, which an ordinary user does not hold.
#[tokio::test]
async fn a_plain_caller_is_refused_by_both_routes() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for path in [PATH, COUNT_PATH] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &f.plain_token, path).await;
        assert_eq!(go_status, 403, "{path}");
        assert_eq!(rs_status, go_status, "{path}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
        assert_eq!(go["id"], "api.context.permissions.app_error");
    }

    // The permission is checked **before** the parameters, so a refused caller sending garbage
    // still gets the 403 and never the 400.
    let path = format!("{PATH}?team_filter=not-an-id");
    let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &f.plain_token, &path).await;
    assert_eq!((go_status, rs_status), (403, 403));
}

/// An unauthenticated request is a 401 from the session middleware, on both routes.
#[tokio::test]
async fn an_anonymous_caller_is_unauthenticated() {
    if !stack_enabled() {
        return;
    }
    let client = client();

    for path in [PATH, COUNT_PATH] {
        let go = client
            .get(format!("{GO}{path}"))
            .send()
            .await
            .expect("Go answers");
        let rs = client
            .get(format!("{RUST}{path}"))
            .send()
            .await
            .expect("mm-api answers");
        assert_eq!(go.status(), 401, "{path}");
        assert_eq!(rs.status(), go.status(), "{path}: statuses must match");
        let go_body = go.bytes().await.expect("a body").to_vec();
        let rs_body = rs.bytes().await.expect("a body").to_vec();
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    }
}

/// A bot is not in the report, and not in the count.
///
/// `getUsersColumns()` is used **bare** here — no `Bots` join, no `IsBot` column — and bots are
/// excluded by a `NOT IN (SELECT UserId FROM Bots)` in the report and by a `Bots.UserId IS NULL`
/// anti-join in the count. Two different spellings of one rule, and neither is reachable from any
/// other assertion in this suite, whose users are all human.
#[tokio::test]
async fn a_bot_is_in_neither_the_report_nor_the_count() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    if !f.planted {
        return;
    }

    let query = format!("page_size=100&search_term={}", f.bot_username);
    assert_eq!(
        ids(&both(&client, &token, &query).await),
        Vec::<String>::new(),
        "the bot's own username finds nobody"
    );
    assert_eq!(
        both_count(&client, &token, &format!("search_term={}", f.bot_username)).await,
        0,
        "and the count agrees, through a differently-spelled exclusion"
    );

    // The search term is doing what the test thinks. Asked directly, the table has exactly one
    // row with that username — so the empty report above is the `Bots` exclusion and not a term
    // that matches nothing. (`POST /users/search` would be the route-level way to ask, and it
    // answers a 500 for a planted bot, which is a different bug in a different place.)
    let pool = fixture_pool().await.expect("the fixture has a database");
    let planted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM users u JOIN bots b ON b.userid = u.id \
                            WHERE u.username = $1",
    )
    .bind(&f.bot_username)
    .fetch_one(&pool)
    .await
    .expect("the count reads");
    assert_eq!(planted, 1, "the bot row is there to be excluded");
}

/// `ReportingMaxPageSize` is **inclusive** — 100 is served and 101 is refused.
#[tokio::test]
async fn the_page_size_bound_is_inclusive() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let rows = both(&client, &token, "page_size=100&search_term=mmrsplainurep").await;
    assert_eq!(
        ids(&rows).len(),
        FIXTURE_USERS as usize,
        "100 is inside the bound"
    );

    let path = format!("{PATH}?page_size=101");
    let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!((go_status, rs_status), (400, 400), "101 is outside it");
}

/// `url.QueryEscape` for the few characters a fixture value can carry.
fn urlencoding(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "+".to_owned(),
            other => format!("%{other:02X}"),
        })
        .collect()
}
