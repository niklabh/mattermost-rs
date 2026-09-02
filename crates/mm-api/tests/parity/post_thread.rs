//! Cross-server parity for `GET /api/v4/posts/{post_id}/thread` — `getPostThread`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity post_thread
//! ```
//!
//! # The fixture is built so that a wrong answer cannot look like the right one
//!
//! - **Five posts in one thread, written alternately by two authors.** One author gives
//!   `Threads.Participants` a single entry, which cannot catch a reversed or truncated list, and
//!   leaves every `ThreadMemberships` row belonging to the caller — so joining that table on the
//!   post's author instead of the session's would answer identically.
//! - **The replies' `create_at` order is not their `id` order and not their insertion order.**
//!   Ids are random base32, so a suite whose posts happen to sort the same way by both keys
//!   cannot tell `ORDER BY CreateAt` from `ORDER BY Id`, and cannot catch a dropped tie-break.
//! - **One reply is edited after it is written**, so its `UpdateAt` is far from its `CreateAt`.
//!   Without that, `updatesOnly` and `fromUpdateAt` select the same rows as their `CreateAt`
//!   siblings and the two cursors are indistinguishable.
//! - **One reply is soft-deleted**, which is the only way the unconditional `DeleteAt = 0` in
//!   both branches is a filter rather than a no-op.
//! - **A second thread lives in a private channel the plain user is not in**, so the 403 and the
//!   404-before-403 ordering have a fixture.
//!
//! # Rows all begin `mmrspostthread`
//!
//! Not the shared `mmrs-parity-` prefix, for the reason `channel_posts` gives: `purge_api_fixtures`
//! runs once per binary and would delete another suite's team mid-run.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client, fetch_both,
    fetch_both_raw, go_minted_token, post_message, stack_enabled,
};

const PREFIX: &str = "mmrspostthread";

/// The one post id this suite mints itself. **Exactly 26 characters** — `IsValidId` and the
/// `varchar(26)` column both require it.
const BROKEN_PROPS_ID: &str = "mmrspostthreadbrokenprops0";

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

async fn fixture_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()
}

/// Remove every row this suite authors, at the **start** of the run — an assertion panics past
/// trailing cleanup, so the next run's purge is the only one certain to happen.
async fn purge_post_thread_fixtures() {
    let Some(pool) = fixture_pool().await else {
        return;
    };

    const TEAMS: &str = "SELECT id FROM teams WHERE name LIKE 'mmrspostthread%'";
    const USERS: &str = "SELECT id FROM users WHERE username LIKE 'mmrspostthread%'";
    let channels = format!("SELECT id FROM channels WHERE teamid IN ({TEAMS})");
    let posts = format!("SELECT id FROM posts WHERE channelid IN ({channels})");

    for statement in [
        format!("DELETE FROM reactions WHERE postid IN ({posts})"),
        format!("DELETE FROM threadmemberships WHERE postid IN ({posts})"),
        format!("DELETE FROM threads WHERE postid IN ({posts})"),
        format!("DELETE FROM postspriority WHERE postid IN ({posts})"),
        format!("DELETE FROM postacknowledgements WHERE postid IN ({posts})"),
        format!("DELETE FROM posts WHERE channelid IN ({channels})"),
        format!(
            "DELETE FROM sidebarchannels WHERE categoryid IN (SELECT id FROM sidebarcategories WHERE teamid IN ({TEAMS}) OR userid IN ({USERS}))"
        ),
        format!("DELETE FROM sidebarcategories WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})"),
        format!("DELETE FROM channelmemberhistory WHERE channelid IN ({channels})"),
        format!(
            "DELETE FROM channelmembers WHERE channelid IN ({channels}) OR userid IN ({USERS})"
        ),
        format!("DELETE FROM publicchannels WHERE teamid IN ({TEAMS})"),
        format!("DELETE FROM channels WHERE teamid IN ({TEAMS})"),
        format!("DELETE FROM teammembers WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})"),
        "DELETE FROM teams WHERE name LIKE 'mmrspostthread%'".to_owned(),
        format!("DELETE FROM sessions WHERE userid IN ({USERS})"),
        "DELETE FROM users WHERE username LIKE 'mmrspostthread%'".to_owned(),
    ] {
        let _ = sqlx::query(&statement).execute(&pool).await;
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

async fn login(client: &reqwest::Client, username: &str, password: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username, "password": password }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 200, "the fixture user cannot log in");
    response
        .headers()
        .get("token")
        .expect("Go returns the session token in a `Token` header")
        .to_str()
        .expect("the token is ASCII")
        .to_owned()
}

struct Fixture {
    /// The thread root, written by the admin in the public channel.
    root: String,
    /// The four replies, in the order they were written. `replies[1]` is the edited one and
    /// `replies[3]` is soft-deleted.
    replies: [String; 4],
    /// `replies[1]`'s `UpdateAt`, which its edit moved well past its `CreateAt`.
    edited_update_at: i64,
    /// `replies[2]`'s `CreateAt` — a cursor that sits strictly inside the thread.
    middle_create_at: i64,
    /// A root in a private channel the plain user is not in.
    private_root: String,
    /// A root of its own, whose `props` column holds a jsonb **array**. Nothing in the REST API
    /// can write one — `postToSlice` marshals a map — and it is the only way to reach the store's
    /// error branch, and therefore the app layer's 500, through a real request.
    broken_props_root: String,
    /// A second, non-admin account: a member of the public channel, not of the private one.
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async { build_fixture(client, token).await })
        .await
}

async fn build_fixture(client: &reqwest::Client, admin_token: &str) -> Fixture {
    purge_post_thread_fixtures().await;

    let team = go_post(
        client,
        admin_token,
        "/api/v4/teams",
        serde_json::json!({
            "name": format!("{PREFIX}team"),
            "display_name": "mmrs post thread",
            "type": "O",
        }),
    )
    .await["id"]
        .as_str()
        .expect("a team id")
        .to_owned();

    let mut channel_ids = Vec::new();
    for (tag, kind) in [("main", "O"), ("private", "P")] {
        let created = go_post(
            client,
            admin_token,
            "/api/v4/channels",
            serde_json::json!({
                "team_id": team,
                "name": format!("{PREFIX}-{tag}"),
                "display_name": format!("mmrs post thread {tag}"),
                "type": kind,
            }),
        )
        .await;
        channel_ids.push(created["id"].as_str().expect("a channel id").to_owned());
    }
    let (channel, private_channel) = (channel_ids[0].clone(), channel_ids[1].clone());

    let plain_username = format!("{PREFIX}plain");
    let plain = go_post(
        client,
        admin_token,
        "/api/v4/users",
        serde_json::json!({
            "email": format!("{plain_username}@mmrs.invalid"),
            "username": plain_username,
            "password": "Mmrs-Plain-1234",
        }),
    )
    .await;
    let plain_id = plain["id"].as_str().expect("a user id").to_owned();
    go_post(
        client,
        admin_token,
        &format!("/api/v4/teams/{team}/members"),
        serde_json::json!({ "team_id": team, "user_id": plain_id }),
    )
    .await;
    add_user_to_channel(client, admin_token, &channel, &plain_id).await;
    let plain_token = login(client, &plain_username, "Mmrs-Plain-1234").await;

    let root = post_message(client, admin_token, &channel, "thread root", None).await;
    // Alternating authors: the plain user writes two of the four, which is what puts two entries
    // in `Threads.Participants` and one `ThreadMemberships` row on each side.
    let reply_a = post_message(client, &plain_token, &channel, "reply a", Some(&root)).await;
    let reply_b = post_message(client, admin_token, &channel, "reply b", Some(&root)).await;
    let reply_c = post_message(client, &plain_token, &channel, "reply c", Some(&root)).await;
    let reply_d = post_message(client, admin_token, &channel, "reply d", Some(&root)).await;

    // Edit `reply_b` so its `UpdateAt` is far from its `CreateAt`. Without a row where the two
    // differ, `fromUpdateAt` and `fromCreateAt` select identically and neither cursor is tested.
    let edited: serde_json::Value = client
        .put(format!("{GO}/api/v4/posts/{reply_b}/patch"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "message": "reply b, edited" }))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the patched post decodes");
    let edited_update_at = edited["update_at"].as_i64().expect("an update_at");

    // Soft-delete the last reply, so `DeleteAt = 0` is a filter.
    let deleted = client
        .delete(format!("{GO}/api/v4/posts/{reply_d}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(deleted.status().is_success(), "deleting the last reply");

    let middle: serde_json::Value = client
        .get(format!("{GO}/api/v4/posts/{reply_c}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the post decodes");
    let middle_create_at = middle["create_at"].as_i64().expect("a create_at");

    let private_root = post_message(
        client,
        admin_token,
        &private_channel,
        "private thread root",
        None,
    )
    .await;
    post_message(
        client,
        admin_token,
        &private_channel,
        "private reply",
        Some(&private_root),
    )
    .await;

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
    insert_broken_props_post(&channel, &admin_id).await;

    Fixture {
        root,
        replies: [reply_a, reply_b, reply_c, reply_d],
        edited_update_at,
        middle_create_at,
        private_root,
        broken_props_root: BROKEN_PROPS_ID.to_owned(),
        plain_token,
    }
}

/// Plant a post whose `props` column is a jsonb **array**, straight through Postgres.
///
/// `StringInterface.Scan` calls `json.Unmarshal` into a `map[string]any`, which rejects an array
/// — so Go answers 500 `app.post.get.app_error`, and so must this port. It is a thread of its
/// own so that it cannot contaminate every other test in the suite.
async fn insert_broken_props_post(channel_id: &str, user_id: &str) {
    let Some(pool) = fixture_pool().await else {
        return;
    };

    let create_at: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(createat), 0) + 1000 FROM posts")
        .fetch_one(&pool)
        .await
        .expect("the clock reads");

    sqlx::query(
        "INSERT INTO posts (id, createat, updateat, editat, deleteat, ispinned, userid, channelid,
                            rootid, originalid, message, type, props, hashtags, filenames, fileids,
                            hasreactions, remoteid)
         VALUES ($1, $2, $2, 0, 0, false, $3, $4, '', '', $5, '', '[1,2]'::jsonb, '', '[]', '[]',
                 false, NULL)",
    )
    .bind(BROKEN_PROPS_ID)
    .bind(create_at)
    .bind(user_id)
    .bind(channel_id)
    .bind("broken props")
    .execute(&pool)
    .await
    .expect("the broken-props row inserts");
}

fn decode(body: &[u8], context: &str) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or_else(|e| panic!("{context}: body is not JSON: {e}"))
}

fn order_of(list: &serde_json::Value) -> Vec<String> {
    list["order"]
        .as_array()
        .expect("order is an array")
        .iter()
        .map(|id| id.as_str().expect("an id").to_owned())
        .collect()
}

/// Fetch a path from both servers and assert the bytes agree, returning Go's decoded body so the
/// test can also assert what the fixture actually produced.
async fn both_agree(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    context: &str,
) -> serde_json::Value {
    let (go, rs) = fetch_both(client, token, path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{context}"
    );
    decode(&go, context)
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The default request: no parameters at all, which Go's own comment says means *all items*.
#[tokio::test]
async fn the_whole_thread_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let list = both_agree(
        &client,
        &token,
        &format!("/api/v4/posts/{}/thread", fixture.root),
        "the whole thread",
    )
    .await;

    // The fixture only proves what it contains. Assert its shape here so that a fixture which
    // silently stopped producing a thread fails *this* test rather than quietly weakening the
    // rest of the suite.
    let order = order_of(&list);
    assert_eq!(
        order.first().map(String::as_str),
        Some(fixture.root.as_str()),
        "the requested post is added first, before the sorted replies"
    );
    for reply in &fixture.replies[..3] {
        assert!(
            order.contains(reply),
            "{reply} is a live reply in {order:?}"
        );
    }
    assert!(
        !order.contains(&fixture.replies[3]),
        "the soft-deleted reply must not appear — `DeleteAt = 0` is unconditional"
    );
    assert_eq!(
        list["has_next"],
        serde_json::json!(false),
        "an unpaginated non-collapsed request still sets has_next"
    );
}

/// `skipFetchThreads` skips the reply query **and** `has_next`: Go assigns the field inside the
/// same block, so the key is absent rather than `false`. Same route, same type, a different set
/// of keys.
#[tokio::test]
async fn skip_fetch_threads_omits_has_next_entirely() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let list = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?skipFetchThreads=true",
            fixture.root
        ),
        "skipFetchThreads",
    )
    .await;

    assert_eq!(
        order_of(&list),
        vec![fixture.root.clone()],
        "only the requested post"
    );
    assert!(
        list.get("has_next").is_none(),
        "has_next is set inside the !skipFetchThreads block, so the key must be absent: {list}"
    );
}

/// The collapsed branch always sets `has_next`, even unpaginated — the opposite of the case
/// above, from the same handler.
#[tokio::test]
async fn collapsed_threads_is_byte_identical_and_always_sets_has_next() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let list = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?collapsedThreads=true",
            fixture.root
        ),
        "collapsedThreads",
    )
    .await;

    assert_eq!(
        list["has_next"],
        serde_json::json!(false),
        "the collapsed branch assigns has_next unconditionally"
    );

    // Without this the test would pass against a handler that ignored the flag: the three
    // columns below are the collapsed branch's whole point and the plain branch never sets them.
    let root = &list["posts"][&fixture.root];
    let participants = root["participants"]
        .as_array()
        .expect("the root carries participants");
    assert_eq!(
        participants.len(),
        2,
        "two authors wrote in this thread, so Threads.Participants has two entries: {root}"
    );
    assert!(
        root.get("is_following").is_some(),
        "is_following comes from the caller's ThreadMemberships row: {root}"
    );
}

/// `perPage` fetches one extra row and reports it as `has_next`. The window includes the root,
/// which is why the page can be shorter than `perPage + 1` entries.
#[tokio::test]
async fn per_page_pages_the_thread_and_sets_has_next() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let list = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?direction=down&perPage=2",
            fixture.root
        ),
        "perPage=2",
    )
    .await;
    assert_eq!(
        list["has_next"],
        serde_json::json!(true),
        "four rows matched a LIMIT of three"
    );

    // A page big enough for the whole thread reports no next page.
    let list = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?direction=down&perPage=50",
            fixture.root
        ),
        "perPage=50",
    )
    .await;
    assert_eq!(list["has_next"], serde_json::json!(false));

    // The boundary itself. Go's guard is `perPage > PerPageMaximum`, so **200 is legal** and 201
    // is the 400 asserted in `every_validation_failure_matches`. Without this case an off-by-one
    // on the comparison — or a wrong `PER_PAGE_MAXIMUM` — is invisible: both mutations survived
    // until it was added.
    both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?direction=down&perPage=200",
            fixture.root
        ),
        "perPage=200, the largest legal page",
    )
    .await;

    // **`perPage` with no `direction` at all.** `direction == ""` is its own SQL statement — Go
    // emits no `ORDER BY` and this port has a second literal for it — so every assertion above,
    // which names a direction, exercises only half the code. Dropping the `+ 1` from the
    // unordered statement's `LIMIT` survived the whole suite until this case existed.
    let list = both_agree(
        &client,
        &token,
        &format!("/api/v4/posts/{}/thread?perPage=2", fixture.root),
        "perPage=2 with no direction",
    )
    .await;
    assert_eq!(
        list["has_next"],
        serde_json::json!(true),
        "the unordered statement pages too"
    );

    // **The `has_next` boundary.** The window is the root plus its three live replies, so
    // `perPage=4` returns exactly four rows against a `LIMIT` of five: `len == perPage + 1` is
    // false and `len >= perPage` is true. Nothing else in the suite separates those two
    // predicates — `perPage=2` makes both true and `perPage=50` makes both false.
    let list = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?direction=down&perPage=4",
            fixture.root
        ),
        "perPage exactly the size of the window",
    )
    .await;
    assert_eq!(
        order_of(&list).len(),
        4,
        "the fixture must have exactly four live posts in this thread, or the boundary moves"
    );
    assert_eq!(
        list["has_next"],
        serde_json::json!(false),
        "four rows against a LIMIT of five is the last page"
    );
}

/// Both directions, and the `ORDER BY` that the unordered request does not emit at all.
#[tokio::test]
async fn both_directions_are_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let down = both_agree(
        &client,
        &token,
        &format!("/api/v4/posts/{}/thread?direction=down", fixture.root),
        "direction=down",
    )
    .await;
    let up = both_agree(
        &client,
        &token,
        &format!("/api/v4/posts/{}/thread?direction=up", fixture.root),
        "direction=up",
    )
    .await;

    // `up` sorts DESC and `down` sorts ASC — the reverse of the intuitive reading, and the one
    // thing a suite that only ever asserted byte equality against a single-post thread could not
    // catch. The root is prepended to `order` before the sorted replies in both cases, so the
    // comparison is over the tail.
    let (down_order, up_order) = (order_of(&down), order_of(&up));
    assert_eq!(
        down_order.first(),
        up_order.first(),
        "the requested post leads both orders"
    );
    let mut reversed = up_order[1..].to_vec();
    reversed.reverse();
    assert_eq!(
        down_order[1..].to_vec(),
        reversed,
        "down is ascending and up is descending over the same rows"
    );
    assert!(
        down_order.len() > 2,
        "a thread this short cannot distinguish the two orders"
    );
}

/// The `CreateAt` cursor, in both directions and with and without the `fromPost` tie-break.
#[tokio::test]
async fn the_create_at_cursor_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let at = fixture.middle_create_at;
    let tie = &fixture.replies[2];
    for query in [
        format!("?direction=down&fromCreateAt={at}"),
        format!("?direction=up&fromCreateAt={at}"),
        format!("?direction=down&fromCreateAt={at}&fromPost={tie}"),
        format!("?direction=up&fromCreateAt={at}&fromPost={tie}"),
        // No direction at all: Go still applies the cursor, and its `else` branch means the
        // comparison is `<` — an unordered request filters *backwards*.
        format!("?fromCreateAt={at}"),
    ] {
        both_agree(
            &client,
            &token,
            &format!("/api/v4/posts/{}/thread{query}", fixture.root),
            &format!("createAt cursor {query}"),
        )
        .await;
    }

    // The cursor has to actually cut the thread, or every request above returns everything and
    // the comparison proves nothing.
    let unfiltered = both_agree(
        &client,
        &token,
        &format!("/api/v4/posts/{}/thread?direction=down", fixture.root),
        "unfiltered",
    )
    .await;
    let filtered = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?direction=down&fromCreateAt={at}",
            fixture.root
        ),
        "filtered",
    )
    .await;
    assert!(
        order_of(&filtered).len() < order_of(&unfiltered).len(),
        "the cursor must exclude something, or it is not being tested"
    );
}

/// **The asymmetry that a shared code path would erase.** `fromUpdateAt` filters in both
/// directions on the non-collapsed branch and **only when `direction == "down"`** on the
/// collapsed one (post_store.go:697 against :845).
#[tokio::test]
async fn the_update_at_cursor_is_direction_gated_on_the_collapsed_branch_only() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let at = fixture.edited_update_at;
    for query in [
        format!("?direction=down&fromUpdateAt={at}"),
        format!("?direction=up&fromUpdateAt={at}"),
        format!("?collapsedThreads=true&direction=down&fromUpdateAt={at}"),
        format!("?collapsedThreads=true&direction=up&fromUpdateAt={at}"),
        format!("?direction=down&fromUpdateAt={at}&updatesOnly=true"),
        format!("?collapsedThreads=true&direction=down&fromUpdateAt={at}&updatesOnly=true"),
        // Both cursors with **no direction**, which is a different SQL statement on both
        // branches. Flipping the comparison in the unordered non-collapsed statement, and
        // inverting the collapsed one's `direction == "down"` gate, both survived the suite
        // until these two cases existed.
        format!("?fromUpdateAt={at}"),
        format!("?collapsedThreads=true&fromUpdateAt={at}"),
    ] {
        both_agree(
            &client,
            &token,
            &format!("/api/v4/posts/{}/thread{query}", fixture.root),
            &format!("updateAt cursor {query}"),
        )
        .await;
    }

    // And name the divergence rather than trusting the byte comparison to have hit it: `up` with
    // a cursor returns *everything* on the collapsed branch and a filtered list on the other.
    let collapsed_up = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?collapsedThreads=true&direction=up&fromUpdateAt={at}",
            fixture.root
        ),
        "collapsed up",
    )
    .await;
    let collapsed_unfiltered = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?collapsedThreads=true&direction=up",
            fixture.root
        ),
        "collapsed up, no cursor",
    )
    .await;
    assert_eq!(
        order_of(&collapsed_up),
        order_of(&collapsed_unfiltered),
        "the collapsed branch drops fromUpdateAt unless direction is down"
    );

    // Unordered, non-collapsed: Go's `else` branch means `<`, so an absent direction filters
    // *backwards* — it is not "no cursor".
    let plain_unordered = both_agree(
        &client,
        &token,
        &format!("/api/v4/posts/{}/thread?fromUpdateAt={at}", fixture.root),
        "unordered updateAt cursor",
    )
    .await;
    let plain_unordered_none = both_agree(
        &client,
        &token,
        &format!("/api/v4/posts/{}/thread", fixture.root),
        "unordered, no cursor",
    )
    .await;
    assert!(
        order_of(&plain_unordered).len() < order_of(&plain_unordered_none).len(),
        "an absent direction still applies fromUpdateAt, with `<`"
    );

    // Unordered, collapsed: the gate is `direction == "down"`, and an absent direction is not
    // "down", so the cursor is dropped entirely.
    let collapsed_unordered = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?collapsedThreads=true&fromUpdateAt={at}",
            fixture.root
        ),
        "unordered collapsed updateAt cursor",
    )
    .await;
    let collapsed_unordered_none = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?collapsedThreads=true",
            fixture.root
        ),
        "unordered collapsed, no cursor",
    )
    .await;
    assert_eq!(
        order_of(&collapsed_unordered),
        order_of(&collapsed_unordered_none),
        "the collapsed branch drops fromUpdateAt for every direction but `down`"
    );

    let plain_up = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?direction=up&fromUpdateAt={at}",
            fixture.root
        ),
        "plain up",
    )
    .await;
    let plain_unfiltered = both_agree(
        &client,
        &token,
        &format!("/api/v4/posts/{}/thread?direction=up", fixture.root),
        "plain up, no cursor",
    )
    .await;
    assert!(
        order_of(&plain_up).len() < order_of(&plain_unfiltered).len(),
        "the non-collapsed branch applies fromUpdateAt in both directions"
    );
}

/// **Asking for a reply's thread means two different things.** The non-collapsed branch resolves
/// `RootId` and returns the whole thread; the collapsed one uses the requested id *literally* as
/// the root and returns the reply alone.
#[tokio::test]
async fn a_replys_collapsed_thread_is_the_reply_by_itself() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let reply = &fixture.replies[0];

    let plain = both_agree(
        &client,
        &token,
        &format!("/api/v4/posts/{reply}/thread"),
        "a reply's plain thread",
    )
    .await;
    let collapsed = both_agree(
        &client,
        &token,
        &format!("/api/v4/posts/{reply}/thread?collapsedThreads=true"),
        "a reply's collapsed thread",
    )
    .await;

    assert!(
        order_of(&plain).len() > 1,
        "the plain branch resolves the reply's root and returns the thread"
    );
    assert_eq!(
        order_of(&collapsed),
        vec![reply.clone()],
        "the collapsed branch queries RootId = <the reply>, which matches nothing"
    );
}

/// **`getPostThread` compares the raw string; `getPostsForChannel` calls `strconv.ParseBool`.**
/// Two handlers, one file, the same four parameter names, forty lines apart. `1`, `t` and `TRUE`
/// are true for the channel page and false here.
#[tokio::test]
async fn the_flags_need_the_literal_string_true() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for spelling in ["1", "t", "T", "TRUE", "True"] {
        let list = both_agree(
            &client,
            &token,
            &format!(
                "/api/v4/posts/{}/thread?skipFetchThreads={spelling}",
                fixture.root
            ),
            &format!("skipFetchThreads={spelling}"),
        )
        .await;
        assert!(
            order_of(&list).len() > 1,
            "?skipFetchThreads={spelling} is not the literal `true`, so the replies are fetched"
        );
    }

    // And the spelling that does work, so the assertion above is not vacuous.
    let list = both_agree(
        &client,
        &token,
        &format!(
            "/api/v4/posts/{}/thread?skipFetchThreads=true",
            fixture.root
        ),
        "skipFetchThreads=true",
    )
    .await;
    assert_eq!(order_of(&list).len(), 1);
}

/// The etag is the raw list's; `If-None-Match` is compared byte for byte and the 304 carries it
/// back.
#[tokio::test]
async fn the_etag_round_trips_to_a_304() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let path = format!("/api/v4/posts/{}/thread", fixture.root);
    let etag = |base: &'static str| {
        let client = client.clone();
        let token = token.clone();
        let path = path.clone();
        async move {
            let response = client
                .get(format!("{base}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("reachable");
            response
                .headers()
                .get("ETag")
                .expect("the 200 carries an ETag")
                .to_str()
                .expect("ASCII")
                .to_owned()
        }
    };
    let (go_etag, rs_etag) = (etag(GO).await, etag(RUST).await);
    assert_eq!(go_etag, rs_etag, "the etags must agree");

    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("If-None-Match", &go_etag)
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status().as_u16(), 304, "{base} should 304");
        assert_eq!(
            response
                .headers()
                .get("ETag")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default(),
            go_etag,
            "{base}'s 304 carries the etag back"
        );
    }
}

/// **The query runs before the permission check**, so a post nobody may read is a 404 when it
/// does not exist and a 403 when it does — the reverse of `getPost`'s ordering.
#[tokio::test]
async fn a_missing_post_is_a_404_and_a_forbidden_one_is_a_403() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    // A well-formed id that names no row: 404 from both, for a caller with no rights to it.
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &fixture.plain_token,
        "/api/v4/posts/aaaaaaaaaaaaaaaaaaaaaaaaaa/thread",
    )
    .await;
    assert_eq!((go_status, rs_status), (404, 404), "a missing post");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a missing post");
    assert_eq!(go["id"], "app.post.get.app_error");

    // A post in a channel the plain user is not in: 403, and only because the row exists.
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &fixture.plain_token,
        &format!("/api/v4/posts/{}/thread", fixture.private_root),
    )
    .await;
    assert_eq!((go_status, rs_status), (403, 403), "a forbidden post");
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a forbidden post");

    // A path segment of the wrong length never reaches the app layer.
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, "/api/v4/posts/short/thread").await;
    assert_eq!((go_status, rs_status), (400, 400), "a malformed id");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a malformed id");
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}

/// All nine validation branches. They share one error id and differ only in the parameter name,
/// which reaches the wire through Go's **translated** `message` — so this suite can tell them
/// apart on Go's side and, until i18n lands, cannot on ours ([D-092]).
#[tokio::test]
async fn every_validation_failure_matches() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let cases = [
        ("?perPage=201", "perPage"),
        ("?perPage=notanumber", "perPage"),
        ("?fromCreateAt=x", "fromCreateAt"),
        (
            "?fromPost=abc",
            "if fromPost is set, then fromCreateAt must also be set",
        ),
        ("?fromUpdateAt=x", "fromUpdateAt"),
        ("?fromCreateAt=1&fromUpdateAt=2", "fromUpdateAt"),
        ("?updatesOnly=true", "fromUpdateAt"),
        ("?direction=sideways", "direction"),
        (
            "?updatesOnly=true&fromUpdateAt=5&direction=up",
            "updatesOnly",
        ),
    ];

    for (query, parameter) in cases {
        let path = format!("/api/v4/posts/{}/thread{query}", fixture.root);
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!((go_status, rs_status), (400, 400), "{query}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, query);
        assert_eq!(
            go["id"], "api.context.invalid_body_param.app_error",
            "{query}"
        );
        // Go's translated message names the parameter, which is how the nine cases stay
        // distinguishable at all — and how a branch wired to the wrong name would be caught.
        assert_eq!(
            go["message"],
            serde_json::json!(format!("Invalid or missing {parameter} in request body.")),
            "{query} must fail on {parameter}"
        );
    }
}

/// A negative `perPage` is forwarded, because Go cannot answer it: `-1` **panics** the Go server
/// (`slice bounds out of range [:-1]`, post_store.go:895, measured — the connection closes with
/// no response) and `-2` and below are a 500 from `pq: bigint out of range`. Forwarding is what
/// keeps a client getting Go's answer, panic included.
#[tokio::test]
async fn a_negative_per_page_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let response = client
        .get(format!(
            "{RUST}/api/v4/posts/{}/thread?perPage=-2",
            fixture.root
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "a negative perPage must reach Go, which is the only server that can produce its answer"
    );
    assert_eq!(response.status().as_u16(), 500);
}

/// `collapsedThreadsExtended` is forwarded — it replaces each stub participant with a
/// `SanitizeProfile`d user, whose output depends on config this server does not read.
#[tokio::test]
async fn collapsed_threads_extended_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let response = client
        .get(format!(
            "{RUST}/api/v4/posts/{}/thread?collapsedThreads=true&collapsedThreadsExtended=true",
            fixture.root
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
    );
    assert_eq!(response.status().as_u16(), 200);
}

/// The writes on this path stay forwarded: only `GET` is registered.
#[tokio::test]
async fn the_route_serves_get_only() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let response = client
        .post(format!("{RUST}/api/v4/posts/{}/thread", fixture.root))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "a POST to a GET-only route falls to the method fallback and is forwarded"
    );
}

/// **The app layer's 500 branch, which nothing else in the suite reaches.** A `props` column
/// holding a jsonb array fails `json.Unmarshal` into `map[string]any` on Go's side and this
/// port's decode on ours, so both answer 500 with the *same* id the 404 uses — swapping the two
/// statuses survived the whole suite until this test existed.
#[tokio::test]
async fn a_props_column_that_is_not_an_object_is_a_500_from_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &token,
        &format!("/api/v4/posts/{}/thread", fixture.broken_props_root),
    )
    .await;
    assert_eq!(
        (go_status, rs_status),
        (500, 500),
        "a non-object props column"
    );
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "broken props");
    assert_eq!(
        go["id"], "app.post.get.app_error",
        "the 500 and the 404 share an id; only the status separates them"
    );
}
