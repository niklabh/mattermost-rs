//! Cross-server parity for `POST /api/v4/teams/{team_id}/posts/search` and
//! `POST /api/v4/posts/search` — `searchPostsInTeam` and `searchPostsInAllTeams`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity post_search
//! ```
//!
//! # The fixture is built so that a wrong answer cannot look like the right one
//!
//! Every message carries the token `mmrsps`, so an AND search can be pinned to this suite's rows
//! on a shared database, and each post carries one word of its own — `bravo`, `foxtrot`,
//! `golf` — so a result set names the rows in it.
//!
//! - **The plain user is a member of two public channels, not of a third public one, and not of
//!   the private one.** The third is what separates the store's membership sub-query from the
//!   app's `FilterPostsByChannelPermissions`: a team member may *read* a public channel, so only
//!   the sub-query keeps its posts out.
//! - **One channel is archived after its post is written**, so `include_deleted_channels` is a
//!   filter and not a no-op. One post is deleted, for `DeleteAt = 0`.
//! - **A direct channel and a group channel exist**, both with `TeamId = ''`, so the
//!   `TeamId = ? OR TeamId = ''` half of the team search has rows to lose, and the `in:@user` and
//!   `in:@a,b,c` operands resolve to something.
//! - **Three dated posts are planted straight into `Posts`** at the last millisecond of one day,
//!   noon of the next and the first millisecond of the one after, so `before:`, `on:` and
//!   `after:` each sit on a boundary and a `<` for a `<=` is a different answer — and a
//!   `time_zone_offset` of +05:30 moves the first of them across the `on:` boundary.
//! - **A `card` post and a `system_generic` post are planted**, since the REST API refuses to
//!   write either, so the two type exclusions are filters.
//! - **105 posts are planted under one word**, so the 100-row cap is a cap.
//! - **A hashtag whose stem matches a shorter tag** (`#mmrspsRunning` against `#mmrspsRun`) is
//!   what makes the exact-match pass after the `tsquery` do something.
//! - **Replies' `create_at` are distinct** — each REST post is separated by a short sleep — so
//!   `ORDER BY CreateAt DESC` never ties, which is what keeps the byte comparison honest
//!   ([`memory: parity suite order-tie flakes`]).
//!
//! # Rows all begin `mmrspostsearch`
//!
//! Not the shared `mmrs-parity-` prefix, for the reason `post_thread` gives: `purge_api_fixtures`
//! runs once per binary and would delete another suite's team mid-run. Planted post ids are the
//! prefix plus twelve digits — exactly 26 characters.
//!
//! # Every request holds the busy read guard
//!
//! Both routes are `DisableWhenBusy`, and `busy_gates` marks this server busy under the write
//! guard; a search made while it does would be a 503 that has nothing to do with search.

use std::time::Duration;

use crate::common;

use common::{
    BUSY_STATE, GO, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_direct_channel, delete_post, fetch_both, fetch_both_raw, fixture_pool, go_minted_token,
    post_both_raw, post_message, stack_enabled, username_of,
};

const PREFIX: &str = "mmrspostsearch";
const PLAIN_PASSWORD: &str = "Mmrs-Plain-1234";

/// 2024-03-14T23:59:59.999Z — the last millisecond of the 14th, UTC.
const DATED_ONE_AT: i64 = 1_710_460_799_999;
/// 2024-03-15T12:00:00.000Z.
const DATED_TWO_AT: i64 = 1_710_504_000_000;
/// 2024-03-16T00:00:00.000Z — the first millisecond of the 16th, UTC.
const DATED_THREE_AT: i64 = 1_710_547_200_000;
/// 2023-01-01T00:00:00.000Z, the base the bulk posts count up from.
const BULK_BASE_AT: i64 = 1_672_531_200_000;
const BULK_COUNT: i64 = 105;
/// +05:30, as the webapp sends it: seconds east of UTC.
const IST_OFFSET: i64 = 19_800;

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

/// A planted post's id: the prefix plus twelve digits, 26 characters.
fn planted_id(n: i64) -> String {
    format!("{PREFIX}{n:012}")
}

/// Remove every row this suite authors, at the **start** of the run — an assertion panics past
/// trailing cleanup, so the next run's purge is the only one certain to happen.
async fn purge_post_search_fixtures() {
    let Some(pool) = fixture_pool().await else {
        return;
    };

    const TEAMS: &str = "SELECT id FROM teams WHERE name LIKE 'mmrspostsearch%'";
    const USERS: &str = "SELECT id FROM users WHERE username LIKE 'mmrspostsearch%'";

    // The direct and group channels have no team, so they are found through their members —
    // and that has to happen before the membership rows go.
    let channel_ids: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT id FROM channels WHERE teamid IN ({TEAMS}) \
         OR (teamid = '' AND id IN (SELECT channelid FROM channelmembers WHERE userid IN ({USERS})))"
    ))
    .fetch_all(&pool)
    .await
    .unwrap_or_default();

    let posts = "SELECT id FROM posts WHERE channelid = ANY($1) OR id LIKE 'mmrspostsearch%'";

    for statement in [
        format!("DELETE FROM reactions WHERE postid IN ({posts})"),
        format!("DELETE FROM threadmemberships WHERE postid IN ({posts})"),
        format!("DELETE FROM threads WHERE postid IN ({posts})"),
        format!("DELETE FROM postspriority WHERE postid IN ({posts})"),
        format!("DELETE FROM postacknowledgements WHERE postid IN ({posts})"),
        "DELETE FROM posts WHERE channelid = ANY($1) OR id LIKE 'mmrspostsearch%'".to_owned(),
        format!(
            "DELETE FROM sidebarchannels WHERE categoryid IN (SELECT id FROM sidebarcategories WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})) OR channelid = ANY($1)"
        ),
        format!("DELETE FROM sidebarcategories WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})"),
        "DELETE FROM channelmemberhistory WHERE channelid = ANY($1)".to_owned(),
        format!("DELETE FROM channelmembers WHERE channelid = ANY($1) OR userid IN ({USERS})"),
        format!("DELETE FROM publicchannels WHERE teamid IN ({TEAMS})"),
        "DELETE FROM channels WHERE id = ANY($1)".to_owned(),
        format!("DELETE FROM teammembers WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})"),
        "DELETE FROM teams WHERE name LIKE 'mmrspostsearch%'".to_owned(),
        format!("DELETE FROM preferences WHERE userid IN ({USERS})"),
        format!("DELETE FROM sessions WHERE userid IN ({USERS})"),
        "DELETE FROM users WHERE username LIKE 'mmrspostsearch%'".to_owned(),
    ] {
        let _ = sqlx::query(&statement)
            .bind(&channel_ids)
            .execute(&pool)
            .await;
    }
}

async fn go_post(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .post(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "POST {path} failed: {}",
        response.text().await.unwrap_or_default()
    );
    response.json().await.expect("the response decodes")
}

async fn login(client: &reqwest::Client, username: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username, "password": PLAIN_PASSWORD }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(
        response.status(),
        200,
        "the fixture user {username} cannot log in"
    );
    response
        .headers()
        .get("token")
        .expect("Go returns the session token in a `Token` header")
        .to_str()
        .expect("the token is ASCII")
        .to_owned()
}

/// Create a user, add them to the team, and answer their id.
async fn create_member(
    client: &reqwest::Client,
    admin_token: &str,
    team: &str,
    username: &str,
) -> String {
    let user = go_post(
        client,
        admin_token,
        "/api/v4/users",
        serde_json::json!({
            "email": format!("{username}@mmrs.invalid"),
            "username": username,
            "password": PLAIN_PASSWORD,
        }),
    )
    .await;
    let id = user["id"].as_str().expect("a user id").to_owned();
    go_post(
        client,
        admin_token,
        &format!("/api/v4/teams/{team}/members"),
        serde_json::json!({ "team_id": team, "user_id": id }),
    )
    .await;
    id
}

async fn create_channel(
    client: &reqwest::Client,
    admin_token: &str,
    team: &str,
    tag: &str,
    kind: &str,
) -> String {
    go_post(
        client,
        admin_token,
        "/api/v4/channels",
        serde_json::json!({
            "team_id": team,
            "name": format!("{PREFIX}-{tag}"),
            "display_name": format!("mmrs post search {tag}"),
            "type": kind,
        }),
    )
    .await["id"]
        .as_str()
        .expect("a channel id")
        .to_owned()
}

/// `post_message`, then a pause so the next post's `CreateAt` is strictly later.
async fn post_spaced(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    message: &str,
    root_id: Option<&str>,
) -> String {
    let id = post_message(client, token, channel_id, message, root_id).await;
    tokio::time::sleep(Duration::from_millis(3)).await;
    id
}

/// Plant a post straight into `Posts`, with a `CreateAt` and a `Type` the REST API would not
/// let a client choose.
async fn plant_post(
    pool: &sqlx::PgPool,
    id: &str,
    channel_id: &str,
    user_id: &str,
    message: &str,
    create_at: i64,
    post_type: &str,
) {
    sqlx::query(
        "INSERT INTO posts (id, createat, updateat, editat, deleteat, ispinned, userid, channelid,
                            rootid, originalid, message, type, props, hashtags, filenames, fileids,
                            hasreactions, remoteid)
         VALUES ($1, $2, $2, 0, 0, false, $3, $4, '', '', $5, $6, '{}'::jsonb, '', '[]', '[]',
                 false, NULL)",
    )
    .bind(id)
    .bind(create_at)
    .bind(user_id)
    .bind(channel_id)
    .bind(message)
    .bind(post_type)
    .execute(pool)
    .await
    .expect("the planted post is written");
}

struct Fixture {
    team: String,
    /// A second team the plain user is not in, holding one `alpha` post.
    team2: String,
    admin_username: String,
    plain_username: String,
    plain_token: String,
    q_username: String,
    r_username: String,
    /// `main`, admin: `mmrsps alpha bravo #mmrspsTag one`, with one reply.
    m1: String,
    /// `main`, plain: `mmrsps alpha charlie t-shirt two`.
    m2: String,
    /// `main`, q: `mmrsps bravo delta exact phrase here three`.
    m3: String,
    /// `main`, q: `mmrsps mmrspsxylophone hotel four`.
    m4: String,
    /// `main`, admin, a reply to `m1`: `mmrsps reply india`.
    m5: String,
    /// `main`, admin: `mmrsps #mmrspsRunning november`.
    m7: String,
    /// `main`, admin: `mmrsps 東京タワー kilo`.
    m8: String,
    /// `other`, admin: `mmrsps alpha echo #mmrspsTag two`.
    o1: String,
    /// `third` (public, plain not a member), admin: `mmrsps alpha third oscar`.
    t1: String,
    /// `private`, admin: `mmrsps alpha foxtrot`.
    p1: String,
    /// `archived`, admin, written before the archive: `mmrsps alpha golf`.
    a1: String,
    /// The admin–plain direct channel, admin: `mmrsps romeo dm`.
    d1: String,
    /// The admin–plain–r group channel, admin: `mmrsps sierra gm`.
    g1: String,
    /// `team2`'s channel, admin: `mmrsps alpha quebec`.
    x1: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async { build_fixture(client, token).await })
        .await
}

async fn build_fixture(client: &reqwest::Client, admin_token: &str) -> Fixture {
    purge_post_search_fixtures().await;

    let admin_id = client
        .get(format!("{GO}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers")
        .json::<serde_json::Value>()
        .await
        .expect("the user decodes")["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let admin_username = username_of(client, admin_token, &admin_id).await;

    let mut teams = Vec::new();
    for tag in ["t1", "t2"] {
        let team = go_post(
            client,
            admin_token,
            "/api/v4/teams",
            serde_json::json!({
                "name": format!("{PREFIX}-{tag}"),
                "display_name": format!("mmrs post search {tag}"),
                "type": "O",
            }),
        )
        .await["id"]
            .as_str()
            .expect("a team id")
            .to_owned();
        teams.push(team);
    }
    let (team, team2) = (teams[0].clone(), teams[1].clone());

    let main = create_channel(client, admin_token, &team, "main", "O").await;
    let other = create_channel(client, admin_token, &team, "other", "O").await;
    let third = create_channel(client, admin_token, &team, "third", "O").await;
    let private = create_channel(client, admin_token, &team, "private", "P").await;
    let archived = create_channel(client, admin_token, &team, "archived", "O").await;
    let t2_main = create_channel(client, admin_token, &team2, "main", "O").await;

    let plain_username = format!("{PREFIX}plain");
    let q_username = format!("{PREFIX}q");
    let r_username = format!("{PREFIX}r");
    let plain_id = create_member(client, admin_token, &team, &plain_username).await;
    let q_id = create_member(client, admin_token, &team, &q_username).await;
    let r_id = create_member(client, admin_token, &team, &r_username).await;
    add_user_to_channel(client, admin_token, &main, &plain_id).await;
    add_user_to_channel(client, admin_token, &other, &plain_id).await;
    add_user_to_channel(client, admin_token, &main, &q_id).await;
    let plain_token = login(client, &plain_username).await;
    let q_token = login(client, &q_username).await;

    let dm = create_direct_channel(client, admin_token, &admin_id, &plain_id).await;
    let gm = go_post(
        client,
        admin_token,
        "/api/v4/channels/group",
        serde_json::json!([admin_id, plain_id, r_id]),
    )
    .await["id"]
        .as_str()
        .expect("a group channel id")
        .to_owned();

    let m1 = post_spaced(
        client,
        admin_token,
        &main,
        "mmrsps alpha bravo #mmrspsTag one",
        None,
    )
    .await;
    let m2 = post_spaced(
        client,
        &plain_token,
        &main,
        "mmrsps alpha charlie t-shirt two",
        None,
    )
    .await;
    let m3 = post_spaced(
        client,
        &q_token,
        &main,
        "mmrsps bravo delta exact phrase here three",
        None,
    )
    .await;
    let m4 = post_spaced(
        client,
        &q_token,
        &main,
        "mmrsps mmrspsxylophone hotel four",
        None,
    )
    .await;
    let m5 = post_spaced(client, admin_token, &main, "mmrsps reply india", Some(&m1)).await;
    let m6 = post_spaced(client, admin_token, &main, "mmrsps alpha juliet", None).await;
    let m7 = post_spaced(
        client,
        admin_token,
        &main,
        "mmrsps #mmrspsRunning november",
        None,
    )
    .await;
    let m8 = post_spaced(client, admin_token, &main, "mmrsps 東京タワー kilo", None).await;
    let o1 = post_spaced(
        client,
        admin_token,
        &other,
        "mmrsps alpha echo #mmrspsTag two",
        None,
    )
    .await;
    let t1 = post_spaced(
        client,
        admin_token,
        &third,
        "mmrsps alpha third oscar",
        None,
    )
    .await;
    let p1 = post_spaced(client, admin_token, &private, "mmrsps alpha foxtrot", None).await;
    let a1 = post_spaced(client, admin_token, &archived, "mmrsps alpha golf", None).await;
    let d1 = post_spaced(client, admin_token, &dm, "mmrsps romeo dm", None).await;
    let g1 = post_spaced(client, admin_token, &gm, "mmrsps sierra gm", None).await;
    let x1 = post_spaced(client, admin_token, &t2_main, "mmrsps alpha quebec", None).await;

    delete_post(client, admin_token, &m6).await;

    let archive = client
        .delete(format!("{GO}/api/v4/channels/{archived}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(archive.status().is_success(), "archiving the channel");

    if let Some(pool) = fixture_pool().await {
        plant_post(
            &pool,
            &planted_id(1),
            &main,
            &admin_id,
            "mmrsps dated one",
            DATED_ONE_AT,
            "",
        )
        .await;
        plant_post(
            &pool,
            &planted_id(2),
            &main,
            &admin_id,
            "mmrsps dated two",
            DATED_TWO_AT,
            "",
        )
        .await;
        plant_post(
            &pool,
            &planted_id(3),
            &main,
            &admin_id,
            "mmrsps dated three",
            DATED_THREE_AT,
            "",
        )
        .await;
        plant_post(
            &pool,
            &planted_id(10),
            &main,
            &admin_id,
            "mmrsps card mike",
            DATED_TWO_AT + 1,
            "card",
        )
        .await;
        plant_post(
            &pool,
            &planted_id(11),
            &main,
            &admin_id,
            "mmrsps system papa",
            DATED_TWO_AT + 2,
            "system_generic",
        )
        .await;
        for i in 0..BULK_COUNT {
            plant_post(
                &pool,
                &planted_id(100 + i),
                &other,
                &admin_id,
                &format!("mmrsps bulk {i}"),
                BULK_BASE_AT + i * 1000,
                "",
            )
            .await;
        }
    }

    Fixture {
        team,
        team2,
        admin_username,
        plain_username,
        plain_token,
        q_username,
        r_username,
        m1,
        m2,
        m3,
        m4,
        m5,
        m7,
        m8,
        o1,
        t1,
        p1,
        a1,
        d1,
        g1,
        x1,
    }
}

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn decode(body: &[u8], context: &str) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "{context}: body is not JSON: {e}\n{}",
            String::from_utf8_lossy(body)
        )
    })
}

fn order_of(list: &serde_json::Value) -> Vec<String> {
    list["order"]
        .as_array()
        .expect("order is an array")
        .iter()
        .map(|id| id.as_str().expect("an id").to_owned())
        .collect()
}

/// POST the body to both servers under the busy read guard.
async fn search_both(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let _not_busy = BUSY_STATE.read().await;
    post_both_raw(client, token, path, body.to_string().as_bytes()).await
}

/// Search on both servers, assert a 200 with byte-identical bodies, and return Go's decoded
/// answer so the test can also assert what the fixture actually produced.
async fn both_agree(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: serde_json::Value,
    context: &str,
) -> serde_json::Value {
    let ((go_status, go), (rs_status, rs)) = search_both(client, token, path, &body).await;
    assert_eq!(
        go_status,
        200,
        "{context}: Go: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        200,
        "{context}: Rust: {}",
        String::from_utf8_lossy(&rs)
    );
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{context}"
    );
    decode(&go, context)
}

/// Search on both servers and assert the same non-200 status with error bodies that agree up
/// to the known gaps.
async fn both_refuse(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &[u8],
    expected: u16,
    context: &str,
) -> serde_json::Value {
    let _not_busy = BUSY_STATE.read().await;
    let ((go_status, go), (rs_status, rs)) = post_both_raw(client, token, path, body).await;
    assert_eq!(
        go_status,
        expected,
        "{context}: Go: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        expected,
        "{context}: Rust: {}",
        String::from_utf8_lossy(&rs)
    );
    assert_error_bodies_match_except_known_gaps(&go, &rs, context)
}

fn team_path(fixture: &Fixture) -> String {
    format!("/api/v4/teams/{}/posts/search", fixture.team)
}

const ALL_TEAMS_PATH: &str = "/api/v4/posts/search";

fn terms(terms: &str) -> serde_json::Value {
    serde_json::json!({ "terms": terms })
}

/// The ids in `order`, as a sorted set, for membership assertions that do not care about order.
fn id_set(list: &serde_json::Value) -> Vec<String> {
    let mut ids = order_of(list);
    ids.sort();
    ids
}

fn sorted(ids: &[&String]) -> Vec<String> {
    let mut out: Vec<String> = ids.iter().map(|id| (*id).clone()).collect();
    out.sort();
    out
}

/// `order` is strictly newest-first by each post's own `create_at`.
fn assert_newest_first(list: &serde_json::Value, context: &str) {
    let order = order_of(list);
    let create_at = |id: &str| {
        list["posts"][id]["create_at"]
            .as_i64()
            .expect("a create_at")
    };
    for pair in order.windows(2) {
        assert!(
            create_at(&pair[0]) > create_at(&pair[1]),
            "{context}: {} is not newer than {}",
            pair[0],
            pair[1]
        );
    }
}

// ---------------------------------------------------------------------------------------------
// tests: the result set
// ---------------------------------------------------------------------------------------------

/// The plain user's view of an AND search: their two public channels, and nothing from the
/// third public channel, the private one, the archived one, the deleted post or the other team.
#[tokio::test]
async fn a_plain_search_is_scoped_to_the_callers_channel_memberships() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let list = both_agree(
        &client,
        &f.plain_token,
        &team_path(f),
        terms("mmrsps alpha"),
        "plain: mmrsps alpha",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m1, &f.m2, &f.o1]));
    assert_newest_first(&list, "plain: mmrsps alpha");
    assert_eq!(list["matches"], serde_json::Value::Null);
    assert_eq!(list["next_post_id"], "");
    assert_eq!(list["prev_post_id"], "");
    // The reply gives `m1` a count, which the correlated sub-query has to produce.
    assert_eq!(list["posts"][&f.m1]["reply_count"], 1);
}

/// The admin reaches the private channel and the third public channel, but not the archived
/// one — that needs the flag.
#[tokio::test]
async fn the_admin_sees_the_private_channel_and_include_deleted_channels_adds_the_archived_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let list = both_agree(
        &client,
        &token,
        &team_path(f),
        terms("mmrsps alpha"),
        "admin: alpha",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m1, &f.m2, &f.o1, &f.t1, &f.p1]));

    let list = both_agree(
        &client,
        &token,
        &team_path(f),
        terms("mmrsps golf"),
        "admin: golf",
    )
    .await;
    assert!(
        order_of(&list).is_empty(),
        "the archived channel is out without the flag"
    );

    let list = both_agree(
        &client,
        &token,
        &team_path(f),
        serde_json::json!({ "terms": "mmrsps golf", "include_deleted_channels": true }),
        "admin: golf, include_deleted_channels",
    )
    .await;
    assert_eq!(order_of(&list), vec![f.a1.clone()]);
}

/// `is_or_search` joins the terms with `|` instead of `&`.
#[tokio::test]
async fn is_or_search_widens_the_join() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let list = both_agree(
        &client,
        &f.plain_token,
        &team_path(f),
        terms("charlie delta"),
        "AND",
    )
    .await;
    assert!(order_of(&list).is_empty(), "no post carries both words");

    let list = both_agree(
        &client,
        &f.plain_token,
        &team_path(f),
        serde_json::json!({ "terms": "charlie delta", "is_or_search": true }),
        "OR",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m2, &f.m3]));
}

/// A hashtag search reads the `Hashtags` column and then insists on an exact, case-folded tag:
/// `#mmrspsRun` stems to the same lexeme as `#mmrspsRunning` and is still not a match.
#[tokio::test]
async fn a_hashtag_search_is_exact_after_the_stemmer() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let list = both_agree(
        &client,
        &f.plain_token,
        &team_path(f),
        terms("#mmrspsTag"),
        "#mmrspsTag",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m1, &f.o1]));

    let list = both_agree(
        &client,
        &f.plain_token,
        &team_path(f),
        terms("#MMRSPSTAG"),
        "#MMRSPSTAG",
    )
    .await;
    assert_eq!(
        id_set(&list),
        sorted(&[&f.m1, &f.o1]),
        "the comparison is case-folded"
    );

    let list = both_agree(
        &client,
        &f.plain_token,
        &team_path(f),
        terms("#mmrspsRun"),
        "#mmrspsRun",
    )
    .await;
    assert!(
        order_of(&list).is_empty(),
        "a stem match is not a tag match"
    );

    let list = both_agree(
        &client,
        &f.plain_token,
        &team_path(f),
        terms("#mmrspsRunning"),
        "#mmrspsRunning",
    )
    .await;
    assert_eq!(order_of(&list), vec![f.m7.clone()]);

    // Plain words and hashtags in one query are two params elements, merged newest-first.
    let list = both_agree(
        &client,
        &f.plain_token,
        &team_path(f),
        terms("#mmrspsTag mmrsps hotel"),
        "mixed",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m1, &f.o1, &f.m4]));
    assert_newest_first(&list, "mixed");
}

/// `-word` is `&!(word)`; on its own it is a tsquery Postgres rejects, which both servers turn
/// into an empty page rather than an error.
#[tokio::test]
async fn excluded_terms_negate_and_alone_are_an_empty_page() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let list = both_agree(
        &client,
        &f.plain_token,
        &team_path(f),
        terms("mmrsps alpha -charlie"),
        "-charlie",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m1, &f.o1]));

    let list = both_agree(
        &client,
        &f.plain_token,
        &team_path(f),
        terms("-alpha"),
        "-alpha alone",
    )
    .await;
    assert!(order_of(&list).is_empty());
    assert_eq!(list["posts"], serde_json::json!({}));
}

/// `from:` and `-from:` resolve a username (with or without `@`) to an id; an unknown one
/// matches nobody.
#[tokio::test]
async fn from_filters_by_author() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = team_path(f);

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!("mmrsps from:{}", f.q_username)),
        "from:q",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m3, &f.m4]));

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!("mmrsps from:@{}", f.q_username)),
        "from:@q",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m3, &f.m4]));

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!("mmrsps alpha -from:{}", f.q_username)),
        "-from:q",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m1, &f.m2, &f.o1]));

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms("mmrsps from:mmrspostsearchnobody"),
        "from:nobody",
    )
    .await;
    assert!(order_of(&list).is_empty());
}

/// `in:` resolves a channel name on the team; `~` is trimmed; an unknown name matches nothing.
#[tokio::test]
async fn in_filters_by_channel_name() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = team_path(f);

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!("mmrsps alpha in:{PREFIX}-other")),
        "in:other",
    )
    .await;
    assert_eq!(order_of(&list), vec![f.o1.clone()]);

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!("mmrsps alpha in:~{PREFIX}-other")),
        "in:~other",
    )
    .await;
    assert_eq!(order_of(&list), vec![f.o1.clone()]);

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!("mmrsps alpha -in:{PREFIX}-other")),
        "-in:other",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m1, &f.m2]));

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!("mmrsps alpha in:{PREFIX}-nowhere")),
        "in:nowhere",
    )
    .await;
    assert!(order_of(&list).is_empty());

    // The channel exists but the caller is not in it: the sub-query, not the app filter, is
    // what keeps `t1` out.
    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!("mmrsps alpha in:{PREFIX}-third")),
        "in:third",
    )
    .await;
    assert!(order_of(&list).is_empty());
}

/// Direct and group channels have no team and are still in a team search, and `in:@user` and
/// `in:@a,b,c` reach them by username.
#[tokio::test]
async fn direct_and_group_channels_are_in_a_team_search() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = team_path(f);

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms("mmrsps romeo"),
        "romeo",
    )
    .await;
    assert_eq!(order_of(&list), vec![f.d1.clone()]);

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!("mmrsps in:@{}", f.admin_username)),
        "in:@admin",
    )
    .await;
    assert_eq!(order_of(&list), vec![f.d1.clone()]);

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!(
            "mmrsps in:@{},{},{}",
            f.admin_username, f.plain_username, f.r_username
        )),
        "in:@admin,plain,r",
    )
    .await;
    assert_eq!(order_of(&list), vec![f.g1.clone()]);

    // Two names is below `ChannelGroupMinUsers`: Go logs the bad-size error and keeps the
    // operand as text, which then matches no channel id.
    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(&format!("mmrsps in:@{},{}", f.plain_username, f.r_username)),
        "in:@plain,r",
    )
    .await;
    assert!(order_of(&list).is_empty());
}

/// A quoted phrase is `<->`-joined, a trailing `*` is a prefix, an internal hyphen is kept, and
/// a CJK term takes the `LIKE` branch.
#[tokio::test]
async fn phrases_prefixes_hyphens_and_cjk() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = team_path(f);

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(r#""exact phrase""#),
        "phrase",
    )
    .await;
    assert_eq!(order_of(&list), vec![f.m3.clone()]);
    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms(r#""phrase exact""#),
        "reversed phrase",
    )
    .await;
    assert!(order_of(&list).is_empty());

    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms("mmrspsxylo*"),
        "prefix",
    )
    .await;
    assert_eq!(order_of(&list), vec![f.m4.clone()]);
    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms("mmrspsxylo"),
        "not a prefix",
    )
    .await;
    assert!(order_of(&list).is_empty());

    let list = both_agree(&client, &f.plain_token, &path, terms("t-shirt"), "hyphen").await;
    assert_eq!(order_of(&list), vec![f.m2.clone()]);

    let list = both_agree(&client, &f.plain_token, &path, terms("東京"), "cjk").await;
    assert_eq!(order_of(&list), vec![f.m8.clone()]);
    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        terms("mmrsps -東京"),
        "cjk excluded",
    )
    .await;
    assert!(
        !order_of(&list).contains(&f.m8),
        "the LIKE exclusion drops the CJK post"
    );
    assert!(order_of(&list).contains(&f.m1));
}

/// The three planted posts sit on the day boundaries: `before:` is `<=` the end of the day
/// before, `after:` is `>=` the start of the day after, `on:` is the day, and the offset moves
/// the day.
#[tokio::test]
async fn date_modifiers_and_the_time_zone_offset() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = team_path(f);
    let (one, two, three) = (planted_id(1), planted_id(2), planted_id(3));

    let list = both_agree(
        &client,
        &token,
        &path,
        terms("mmrsps dated on:2024-03-15"),
        "on",
    )
    .await;
    assert_eq!(order_of(&list), vec![two.clone()]);

    let list = both_agree(
        &client,
        &token,
        &path,
        serde_json::json!({ "terms": "mmrsps dated on:2024-03-15", "time_zone_offset": IST_OFFSET }),
        "on, IST",
    )
    .await;
    assert_eq!(
        order_of(&list),
        vec![two.clone(), one.clone()],
        "23:59Z on the 14th is the 15th in IST"
    );

    let list = both_agree(
        &client,
        &token,
        &path,
        terms("mmrsps dated after:2024-03-15"),
        "after",
    )
    .await;
    assert_eq!(
        order_of(&list),
        vec![three.clone()],
        "the first millisecond of the 16th is after the 15th"
    );

    let list = both_agree(
        &client,
        &token,
        &path,
        terms("mmrsps dated before:2024-03-15"),
        "before",
    )
    .await;
    assert_eq!(
        order_of(&list),
        vec![one.clone()],
        "the last millisecond of the 14th is before the 15th"
    );

    let list = both_agree(
        &client,
        &token,
        &path,
        terms("mmrsps dated -on:2024-03-15"),
        "-on",
    )
    .await;
    assert_eq!(order_of(&list), vec![three.clone(), one.clone()]);

    let list = both_agree(
        &client,
        &token,
        &path,
        terms("mmrsps dated -after:2024-03-15"),
        "-after",
    )
    .await;
    assert_eq!(order_of(&list), vec![two.clone(), one.clone()]);

    let list = both_agree(
        &client,
        &token,
        &path,
        terms("mmrsps dated -before:2024-03-15"),
        "-before",
    )
    .await;
    assert_eq!(order_of(&list), vec![three.clone(), two.clone()]);

    // `on:` wins over everything else in the same query.
    let list = both_agree(
        &client,
        &token,
        &path,
        terms("mmrsps dated on:2024-03-15 after:2024-03-15"),
        "on beats after",
    )
    .await;
    assert_eq!(order_of(&list), vec![two.clone()]);
}

/// `page` beyond 0 is empty, `per_page` is ignored, and the window is 100 rows.
#[tokio::test]
async fn paging_is_a_hundred_row_window_on_page_zero() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = team_path(f);

    let list = both_agree(&client, &token, &path, terms("mmrsps bulk"), "bulk").await;
    assert_eq!(order_of(&list).len(), 100, "105 planted, 100 returned");
    assert_newest_first(&list, "bulk");
    assert_eq!(order_of(&list)[0], planted_id(100 + BULK_COUNT - 1));

    let list = both_agree(
        &client,
        &token,
        &path,
        serde_json::json!({ "terms": "mmrsps bulk", "per_page": 5 }),
        "bulk, per_page 5",
    )
    .await;
    assert_eq!(
        order_of(&list).len(),
        100,
        "per_page is not read by the database search"
    );

    let list = both_agree(
        &client,
        &token,
        &path,
        serde_json::json!({ "terms": "mmrsps bulk", "page": 1, "per_page": 5 }),
        "bulk, page 1",
    )
    .await;
    assert!(order_of(&list).is_empty());

    let list = both_agree(
        &client,
        &token,
        &path,
        serde_json::json!({ "terms": "mmrsps bulk", "page": -1 }),
        "bulk, page -1",
    )
    .await;
    assert_eq!(order_of(&list).len(), 100, "a negative page is page zero");
}

/// System posts, `card` posts, deleted posts and burn-on-read posts never come back; a reply
/// does, carrying its thread's count.
#[tokio::test]
async fn excluded_post_types_and_the_reply() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = team_path(f);

    for (word, why) in [
        ("mike", "a card post"),
        ("papa", "a system post"),
        ("juliet", "a deleted post"),
    ] {
        let list = both_agree(
            &client,
            &token,
            &path,
            terms(&format!("mmrsps {word}")),
            why,
        )
        .await;
        assert!(order_of(&list).is_empty(), "{why} is not searchable");
    }

    let list = both_agree(&client, &token, &path, terms("mmrsps india"), "the reply").await;
    assert_eq!(order_of(&list), vec![f.m5.clone()]);
    assert_eq!(list["posts"][&f.m5]["root_id"], f.m1);
    assert_eq!(list["posts"][&f.m5]["reply_count"], 1);

    let list = both_agree(&client, &token, &path, terms("*"), "star").await;
    assert!(
        order_of(&list).is_empty(),
        "`*` alone is dropped before the store"
    );
}

// ---------------------------------------------------------------------------------------------
// tests: the two routes, permissions, and the 400s
// ---------------------------------------------------------------------------------------------

/// The all-teams route: the plain user still cannot see the other team; the admin can.
#[tokio::test]
async fn the_all_teams_route_crosses_teams_the_caller_is_in() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let list = both_agree(
        &client,
        &f.plain_token,
        ALL_TEAMS_PATH,
        terms("mmrsps alpha"),
        "plain, all teams",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&f.m1, &f.m2, &f.o1]));

    let list = both_agree(
        &client,
        &token,
        ALL_TEAMS_PATH,
        terms("mmrsps alpha"),
        "admin, all teams",
    )
    .await;
    assert_eq!(
        id_set(&list),
        sorted(&[&f.m1, &f.m2, &f.o1, &f.t1, &f.p1, &f.x1])
    );
    assert_newest_first(&list, "admin, all teams");

    // `in:` on the all-teams route resolves the name against **no** team, and Go's
    // `getByName` (channel_store.go:1690) is `TeamId = ? OR TeamId = ''` — so with an empty
    // team only a direct or group channel can match a name. The operand stays a name, matches
    // no channel id, and the page is empty on both servers: the cross-team ambiguity Go's
    // `TODO` above `convertChannelNamesToChannelIds` records, measured.
    let list = both_agree(
        &client,
        &token,
        ALL_TEAMS_PATH,
        terms(&format!("mmrsps alpha in:{PREFIX}-other")),
        "all teams, in:other",
    )
    .await;
    assert!(
        order_of(&list).is_empty(),
        "a team channel's name cannot be resolved without a team: {:?}",
        order_of(&list)
    );
}

/// A team the caller is not in, and a team that does not exist, are the same 403 — before the
/// body is read.
#[tokio::test]
async fn a_team_the_caller_is_not_in_is_forbidden() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/teams/{}/posts/search", f.team2);
    let body = both_refuse(
        &client,
        &f.plain_token,
        &path,
        br#"{"terms":"mmrsps alpha"}"#,
        403,
        "plain on team2",
    )
    .await;
    assert_eq!(body["id"], "api.context.permissions.app_error");

    both_refuse(
        &client,
        &f.plain_token,
        &path,
        b"{",
        403,
        "plain on team2, malformed body",
    )
    .await;

    let path = "/api/v4/teams/zzzzzzzzzzzzzzzzzzzzzzzzzz/posts/search";
    both_refuse(
        &client,
        &f.plain_token,
        path,
        br#"{"terms":"mmrsps alpha"}"#,
        403,
        "no such team",
    )
    .await;
}

/// Every way to fail the body: missing or empty `terms`, a `null` document, no document at all,
/// malformed JSON and a wrongly-typed field.
#[tokio::test]
async fn the_body_failures_are_gos_400s() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let path = team_path(f);

    for (body, id, why) in [
        (
            br#"{}"#.as_slice(),
            "api.context.invalid_body_param.app_error",
            "no terms",
        ),
        (
            br#"{"terms":""}"#,
            "api.context.invalid_body_param.app_error",
            "empty terms",
        ),
        (
            br#"{"terms":null}"#,
            "api.context.invalid_body_param.app_error",
            "null terms",
        ),
        (
            b"null",
            "api.context.invalid_body_param.app_error",
            "a null document",
        ),
        (
            b"",
            "api.post.search_posts.invalid_body.app_error",
            "no document",
        ),
        (
            b"{",
            "api.post.search_posts.invalid_body.app_error",
            "malformed",
        ),
        (
            br#"{"terms":5}"#,
            "api.post.search_posts.invalid_body.app_error",
            "terms is a number",
        ),
        (
            br#"{"terms":"x","page":"1"}"#,
            "api.post.search_posts.invalid_body.app_error",
            "page is a string",
        ),
        (
            br#"{"terms":"x","page":1.5}"#,
            "api.post.search_posts.invalid_body.app_error",
            "page is a float",
        ),
        (
            br#"{"terms":"x","is_or_search":"yes"}"#,
            "api.post.search_posts.invalid_body.app_error",
            "is_or_search is a string",
        ),
    ] {
        for route in [path.as_str(), ALL_TEAMS_PATH] {
            let answer = both_refuse(
                &client,
                &f.plain_token,
                route,
                body,
                400,
                &format!("{why} on {route}"),
            )
            .await;
            assert_eq!(answer["id"], id, "{why} on {route}");
        }
    }

    // One value is read and the rest of the body is not: trailing bytes are not an error.
    let list = both_agree(
        &client,
        &f.plain_token,
        &path,
        serde_json::json!({ "terms": "mmrsps romeo" }),
        "trailing",
    )
    .await;
    assert_eq!(order_of(&list), vec![f.d1.clone()]);
    let ((go_status, go), (rs_status, rs)) = {
        let _not_busy = BUSY_STATE.read().await;
        post_both_raw(
            &client,
            &f.plain_token,
            &path,
            br#"{"terms":"mmrsps romeo"} trailing"#,
        )
        .await
    };
    assert_eq!(
        (go_status, rs_status),
        (200, 200),
        "trailing bytes after the first value"
    );
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
}

/// Registering the literal `/posts/search` beside `/posts/{post_id}` must not take the other
/// methods away: `GET /posts/search` is still the 400 `get_post` gives a non-id, and a real post
/// is still served.
#[tokio::test]
async fn the_post_id_neighbours_are_still_answered() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &token, "/api/v4/posts/search").await;
    assert_eq!((go_status, rs_status), (400, 400));
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, "GET /posts/search");
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");

    let (go, rs) = fetch_both(&client, &token, &format!("/api/v4/posts/{}", f.m1)).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
}
