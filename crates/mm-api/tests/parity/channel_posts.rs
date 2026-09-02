//! Cross-server parity for `GET /api/v4/channels/{channel_id}/posts` — `getPostsForChannel`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity_channel_posts
//! ```
//!
//! # The fixture is built so that a wrong answer cannot look like the right one
//!
//! - **Two of the six posts are replies to the *same* root**, and that root is the oldest post in
//!   the channel. So a `per_page=2` window holds replies whose root is off the page, which is the
//!   only way `getParentsPosts` becomes observable: its rows reach `posts` and never `order`, and
//!   a port that skipped the second query answers with an `order` that is right and a `posts` map
//!   that is short.
//! - **The channel has a second author**, so `Threads.Participants` has two entries and its
//!   *order* is checkable. A single-participant thread cannot catch a reversed list.
//! - **One post carries a reaction and a custom-emoji name**, so `metadata.reactions` and
//!   `metadata.emojis` are non-empty for exactly one post and empty for the rest.
//! - **One post is soft-deleted**, which is what makes `include_deleted` a *difference* rather
//!   than a no-op, and what exercises the deleted-post short circuit inside the metadata pipeline.
//! - **One post is inserted straight into Postgres with a NULL `props` column.** Nothing in the
//!   REST API can produce one — `postToSlice` writes `{}` for a nil map — and it is what found
//!   the divergence described below.
//!
//! # `MakeNonNil` runs on one branch only, and it turns out not to matter
//!
//! `GetPosts` ends with `list.MakeNonNil()` and `getPostsCollapsedThreads` does not, so the
//! obvious prediction is `"props":{}` from a plain page and `"props":null` from a collapsed one
//! for a post whose column is NULL. **Go answers `{}` to both**, and to `GET /posts/{id}` as
//! well, because the value was never nil: sqlx allocates a nil map field before scanning into
//! it, so `StringInterface.Scan`'s early return on NULL lands on an empty map.
//!
//! This suite found that by predicting the difference and measuring the opposite — the port had
//! shipped `"props":null` from `getPost` since that route landed. See
//! [`a_null_props_column_is_an_empty_object_on_every_route`] and the notes in
//! `mm_store::post_store`.
//!
//! # Fixture rows all begin `mmrschanposts`
//!
//! Not the shared `mmrs-parity-` prefix: `common::purge_api_fixtures` runs once per test
//! *binary* and binaries run concurrently, so sharing the prefix lets another suite's start-up
//! delete this suite's team mid-run. [`purge_channel_post_fixtures`] clears these instead.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client, fetch_both,
    fetch_both_raw, go_minted_token, post_message, stack_enabled,
};

const PREFIX: &str = "mmrschanposts";

/// The one post id this suite mints itself. **Exactly 26 characters**, because that is what
/// `IsValidId` and the `varchar(26)` column both require.
const NULL_PROPS_ID: &str = "mmrschanpostsnullprops0000";

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

/// Remove every row this suite authors, at the **start** of the run: an assertion panics past
/// trailing cleanup, so the next run's purge is the only one certain to happen.
///
/// Selection is by team rather than by name wherever it can be, because Go authors
/// `town-square`, `off-topic` and a set of `SidebarCategories` on this suite's behalf and none of
/// them carries the prefix — the gap [D-155] records.
async fn purge_channel_post_fixtures() {
    let Some(pool) = fixture_pool().await else {
        return;
    };

    const TEAMS: &str = "SELECT id FROM teams WHERE name LIKE 'mmrschanposts%'";
    const USERS: &str = "SELECT id FROM users WHERE username LIKE 'mmrschanposts%'";
    let channels = format!("SELECT id FROM channels WHERE teamid IN ({TEAMS})");

    for statement in [
        format!(
            "DELETE FROM reactions WHERE postid IN (SELECT id FROM posts WHERE channelid IN ({channels}))"
        ),
        format!(
            "DELETE FROM threadmemberships WHERE postid IN (SELECT id FROM posts WHERE channelid IN ({channels}))"
        ),
        format!(
            "DELETE FROM threads WHERE postid IN (SELECT id FROM posts WHERE channelid IN ({channels}))"
        ),
        format!(
            "DELETE FROM postspriority WHERE postid IN (SELECT id FROM posts WHERE channelid IN ({channels}))"
        ),
        format!(
            "DELETE FROM postacknowledgements WHERE postid IN (SELECT id FROM posts WHERE channelid IN ({channels}))"
        ),
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
        "DELETE FROM teams WHERE name LIKE 'mmrschanposts%'".to_owned(),
        format!("DELETE FROM sessions WHERE userid IN ({USERS})"),
        "DELETE FROM users WHERE username LIKE 'mmrschanposts%'".to_owned(),
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

/// The whole fixture, built once per binary.
struct Fixture {
    channel: String,
    /// A root written by the *other* user, with a reply from that same user — a thread the
    /// caller neither wrote nor follows.
    foreign_root: String,
    /// The root carrying a reaction, a `PostsPriority` row and a `PostAcknowledgements` row —
    /// the only post on the page whose `metadata` is not empty.
    reacted: String,
    /// The oldest post, and the root of both replies.
    root: String,
    /// The two replies, in the order they were written.
    replies: [String; 2],
    /// The post that was soft-deleted after it was written.
    doomed: String,
    /// The newest ordinary post, so `?before=` and `?after=` have something to name.
    newest: String,
    /// A private channel the plain user is deliberately *not* in, emptied of the system post Go
    /// writes when its creator joins — the only way to get a channel with **no** posts at all,
    /// and therefore the only way to reach the clock-stamped etag.
    private_channel: String,
    /// A channel holding one message with a link in it, which the metadata pipeline refuses.
    link_channel: String,
    /// A second, non-admin account that is a member of `channel` but not of `private_channel`.
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async { build_fixture(client, token).await })
        .await
}

async fn build_fixture(client: &reqwest::Client, admin_token: &str) -> Fixture {
    purge_channel_post_fixtures().await;

    let team = go_post(
        client,
        admin_token,
        "/api/v4/teams",
        serde_json::json!({
            "name": format!("{PREFIX}team"),
            "display_name": "mmrs chan posts",
            "type": "O",
        }),
    )
    .await["id"]
        .as_str()
        .expect("a team id")
        .to_owned();

    let mut channel_ids = Vec::new();
    for (tag, kind) in [("main", "O"), ("private", "P"), ("link", "O")] {
        let created = go_post(
            client,
            admin_token,
            "/api/v4/channels",
            serde_json::json!({
                "team_id": team,
                "name": format!("{PREFIX}-{tag}"),
                "display_name": format!("mmrs chan posts {tag}"),
                "type": kind,
            }),
        )
        .await;
        channel_ids.push(created["id"].as_str().expect("a channel id").to_owned());
    }
    let (channel, private_channel, link_channel) = (
        channel_ids[0].clone(),
        channel_ids[1].clone(),
        channel_ids[2].clone(),
    );

    // A second author, so a thread has two participants and the list's order is checkable.
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

    // Oldest first. The root is first so that a small page cuts its thread in half.
    let root = post_message(client, admin_token, &channel, "root one", None).await;
    let reacted = post_message(
        client,
        admin_token,
        &channel,
        "root two :mmrsmissing:",
        None,
    )
    .await;
    let reply_a = post_message(client, &plain_token, &channel, "reply a", Some(&root)).await;
    post_message(client, admin_token, &channel, "root three", None).await;
    let reply_b = post_message(client, admin_token, &channel, "reply b", Some(&root)).await;
    let doomed = post_message(client, admin_token, &channel, "doomed", None).await;
    // A thread the caller is not in: the other user starts it *and* replies to it, so there is a
    // `ThreadMemberships` row for them and none for the admin. Without it every membership row on
    // the page belongs to the caller, and joining `ThreadMemberships` on the wrong user id — the
    // post's author instead of the session's — produces the same answer for every row.
    let foreign_root = post_message(client, &plain_token, &channel, "their root", None).await;
    post_message(
        client,
        &plain_token,
        &channel,
        "their reply",
        Some(&foreign_root),
    )
    .await;
    let newest = post_message(client, admin_token, &channel, "root four", None).await;

    let admin_id = go_get(client, admin_token, "/api/v4/users/me").await["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    go_post(
        client,
        admin_token,
        "/api/v4/reactions",
        serde_json::json!({ "user_id": admin_id, "post_id": reacted, "emoji_name": "grinning" }),
    )
    .await;

    let deleted = client
        .delete(format!("{GO}/api/v4/posts/{doomed}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(deleted.status().is_success(), "deleting the doomed post");

    post_message(
        client,
        admin_token,
        &link_channel,
        "look at https://example.invalid/thing",
        None,
    )
    .await;

    insert_null_props_post(&channel, &admin_id).await;
    // Planted on the **root** rather than on the reacted post, so that a narrow page can put
    // it in `posts` without putting it in `order` — see the batch-read test.
    plant_priority_and_acknowledgement(&root, &channel, &admin_id).await;
    empty_the_channel(&private_channel).await;

    Fixture {
        channel,
        foreign_root,
        reacted,
        root,
        replies: [reply_a, reply_b],
        doomed,
        newest,
        private_channel,
        link_channel,
        plain_token,
    }
}

async fn go_get(client: &reqwest::Client, token: &str, path: &str) -> serde_json::Value {
    client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the response decodes")
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
        .expect("Go returns a token header")
        .to_str()
        .expect("ASCII")
        .to_owned()
}

/// The one row the REST API cannot write: a post whose `Props` column is SQL NULL.
///
/// `postToSlice` (post_store.go:82) marshals a nil `Props` to the string `{}`, so every post Go
/// creates has a JSON object there. NULL is still reachable — older rows, an import, a plugin
/// writing directly — and `StringInterface.Scan` returns nil for it, which is what makes the
/// `MakeNonNil` asymmetry visible.
///
/// **Every other column is written explicitly**, including the ones a lazier insert would leave
/// NULL. Go's `model.Post` scans them into non-pointer fields, and a NULL there makes Go's own
/// `GET /channels/{id}/posts` fail for this channel — the failure mode [D-157] closed for
/// `Users`, reproduced here so it does not come back for `Posts`.
async fn insert_null_props_post(channel_id: &str, user_id: &str) {
    let Some(pool) = fixture_pool().await else {
        return;
    };

    // Newer than every post above, so its position in `order` is fixed rather than racing the
    // millisecond clock the API-created posts were stamped with.
    let create_at: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(createat), 0) + 1000 FROM posts")
        .fetch_one(&pool)
        .await
        .expect("the clock reads");

    sqlx::query(
        "INSERT INTO posts (id, createat, updateat, editat, deleteat, ispinned, userid, channelid,
                            rootid, originalid, message, type, props, hashtags, filenames, fileids,
                            hasreactions, remoteid)
         VALUES ($1, $2, $2, 0, 0, false, $3, $4, '', '', $5, '', NULL, '', '[]', '[]', false, NULL)",
    )
    .bind(NULL_PROPS_ID)
    .bind(create_at)
    .bind(user_id)
    .bind(channel_id)
    .bind("null props")
    .execute(&pool)
    .await
    .expect("the NULL-props row inserts");
}

/// Plant a `PostsPriority` and a `PostAcknowledgements` row, straight through Postgres.
///
/// Both write paths are licence-gated on this Team Edition build
/// (`license_error.feature_unavailable`) and the **read** path is not — `IsPostPriorityEnabled`
/// consults `ServiceSettings.PostPriority` and no licence at all. Writing the rows directly is
/// the only way to give `GetPriorityForPostList` and `GetAcknowledgementsForPostList` an oracle;
/// without them the two batch queries ship with nothing behind them.
///
/// The acknowledgement's timestamp is deliberately not a `CreateAt` of anything, so a port
/// reading the wrong column is visible rather than coincidentally right.
async fn plant_priority_and_acknowledgement(post_id: &str, channel_id: &str, user_id: &str) {
    let Some(pool) = fixture_pool().await else {
        return;
    };

    let _ = sqlx::query(
        "INSERT INTO postspriority (postid, channelid, priority, requestedack, persistentnotifications)
         VALUES ($1, $2, 'urgent', true, false) ON CONFLICT (postid) DO NOTHING",
    )
    .bind(post_id)
    .bind(channel_id)
    .execute(&pool)
    .await;

    let _ = sqlx::query(
        "INSERT INTO postacknowledgements (postid, userid, acknowledgedat, remoteid, channelid)
         VALUES ($1, $2, 1700000000123, '', $3) ON CONFLICT (postid, userid) DO NOTHING",
    )
    .bind(post_id)
    .bind(user_id)
    .bind(channel_id)
    .execute(&pool)
    .await;
}

/// Remove every post from a channel, straight through Postgres.
///
/// Creating a channel makes Go write a `system_join_channel` post for its creator, so there is no
/// way to get an empty channel through the API — and without one, the branch of `GetEtag` that
/// stamps the clock instead of a stored `UpdateAt` is unreachable.
async fn empty_the_channel(channel_id: &str) {
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let _ = sqlx::query("DELETE FROM posts WHERE channelid = $1")
        .bind(channel_id)
        .execute(&pool)
        .await;
}

/// Assert a shape is **forwarded** and that the forwarded answer still matches Go's own.
async fn assert_forwarded_and_identical(client: &reqwest::Client, token: &str, path: &str) {
    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        (status, served_by, body)
    };

    let (go_status, _, go_body) = get(GO).await;
    let (rs_status, served_by, rs_body) = get(RUST).await;

    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "{path}: this shape must be forwarded"
    );
    assert_eq!(go_status, rs_status, "{path}: status");
    assert_eq!(
        without_request_id(go_body),
        without_request_id(rs_body),
        "{path}: forwarded body"
    );
}

/// `request_id` is minted per request, so two calls to the *same* server disagree on it. Every
/// other key is compared.
fn without_request_id(mut body: serde_json::Value) -> serde_json::Value {
    if let Some(object) = body.as_object_mut()
        && object.contains_key("status_code")
    {
        object.remove("request_id");
    }
    body
}

/// `(go_status, rust_status)` for a path, with **no** claim about which server answered — for the
/// cases where the interesting question is the status and the Rust side may forward.
async fn statuses(client: &reqwest::Client, token: &str, path: &str) -> (u16, u16) {
    let get = async |base: &str| {
        client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"))
            .status()
            .as_u16()
    };
    (get(GO).await, get(RUST).await)
}

/// `ETag` from both servers for the same path, with the Rust side's `x-mmrs-served-by` asserted.
async fn etags(client: &reqwest::Client, token: &str, path: &str) -> (String, String) {
    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the server answers");
        assert_eq!(response.status(), 200, "{base}{path}");
        if base == RUST {
            assert_eq!(
                response
                    .headers()
                    .get("x-mmrs-served-by")
                    .and_then(|v| v.to_str().ok()),
                Some("rust"),
                "{path} was forwarded, so its etag is Go's and proves nothing"
            );
        }
        response
            .headers()
            .get("etag")
            .expect("an ETag header")
            .to_str()
            .expect("ASCII")
            .to_owned()
    };

    (get(GO).await, get(RUST).await)
}

fn decode(body: &[u8], context: &str) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or_else(|e| panic!("{context}: not JSON: {e}"))
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The request a client makes when it opens a channel, byte for byte.
#[tokio::test]
async fn a_channel_page_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/posts", fixture.channel);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "the plain page"
    );

    // The fixture only proves what it contains. Both replies, the deleted post's absence and the
    // NULL-props row are asserted here so a fixture that silently stopped producing them fails
    // *this* test rather than quietly weakening every other one.
    // The channel also holds the `system_join_channel` posts Go writes on its own, so the
    // assertions name ids rather than counting rows — a count here would break the next time
    // upstream adds a system post and would say nothing about this route.
    let list = decode(&go, "the plain page");
    let order: Vec<&str> = list["order"]
        .as_array()
        .expect("an order")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    for id in [
        fixture.root.as_str(),
        fixture.replies[0].as_str(),
        fixture.replies[1].as_str(),
        fixture.newest.as_str(),
        NULL_PROPS_ID,
    ] {
        assert!(order.contains(&id), "{id} should be on the page");
    }
    assert!(
        !order.contains(&fixture.doomed.as_str()),
        "the soft-deleted post is not on a page that did not ask for it"
    );
    assert_eq!(
        order.first(),
        Some(&NULL_PROPS_ID),
        "newest first — the hand-inserted row is stamped after every other post"
    );
}

/// The collapsed-threads page: roots only, with the thread columns filled in.
#[tokio::test]
async fn collapsed_threads_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let path = format!(
        "/api/v4/channels/{}/posts?collapsedThreads=true",
        fixture.channel
    );
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "the collapsed-threads page"
    );

    // Without these the test would pass just as well against a handler that ignored the flag.
    let list = decode(&go, "the collapsed-threads page");
    let order: Vec<&str> = list["order"]
        .as_array()
        .expect("an order")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        order.contains(&fixture.root.as_str()),
        "roots are on the page"
    );
    for reply in &fixture.replies {
        assert!(
            !order.contains(&reply.as_str()),
            "a reply is not a root and has no place in a collapsed order"
        );
    }
    let root = &list["posts"][&fixture.root];
    assert_eq!(root["reply_count"], 2, "the thread's count, not the post's");
    assert!(
        root["last_reply_at"].as_i64().is_some_and(|at| at > 0),
        "last_reply_at comes from Threads and is filled only on this branch"
    );
    assert_eq!(
        root["participants"].as_array().expect("participants").len(),
        2,
        "both authors of the thread"
    );
    assert_eq!(
        root["is_following"], true,
        "the caller wrote the root, so ThreadMemberships.Following is true"
    );

    // The other user's thread. `is_following` is a `*bool` with `omitempty`, so no membership row
    // for *this* caller means the key is absent — not `false`. Joining on the post's author
    // instead of on the session would put `true` here.
    let theirs = &list["posts"][&fixture.foreign_root];
    assert_eq!(theirs["reply_count"], 1, "their thread has one reply");
    assert!(
        theirs.get("is_following").is_none(),
        "the caller follows nothing here, so the key is omitted: {theirs}"
    );
}

/// A NULL `props` column is `{}` on **every** route, and the port had it as `null` until this
/// test measured it.
///
/// Three requests rather than one: `MakeNonNil` runs on the plain page and not on the collapsed
/// one, so the collapsed page is what shows the empty map does not come from there, and
/// `GET /posts/{id}` — a different store query, a different handler — is what shows it does not
/// come from the list pipeline either. It is sqlx allocating the map before it scans; see
/// `mm_store::post_store`.
#[tokio::test]
async fn a_null_props_column_is_an_empty_object_on_every_route() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    for path in [
        format!("/api/v4/channels/{}/posts", fixture.channel),
        format!(
            "/api/v4/channels/{}/posts?collapsedThreads=true",
            fixture.channel
        ),
    ] {
        let (go, rs) = fetch_both(&client, &token, &path).await;
        assert_eq!(
            decode(&go, &path)["posts"][NULL_PROPS_ID]["props"],
            serde_json::json!({}),
            "{path}: Go answers an empty object for a NULL column"
        );
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
    }

    // The single-post route, which is where the divergence had been shipping.
    let single = format!("/api/v4/posts/{NULL_PROPS_ID}");
    let (go, rs) = fetch_both(&client, &token, &single).await;
    assert_eq!(
        decode(&go, &single)["props"],
        serde_json::json!({}),
        "getPost answers the same empty object"
    );
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{single}"
    );
}

/// The two batch reads behind `metadata.priority` and `metadata.acknowledgements`.
///
/// `PreparePostListForClient` does **not** ask for them per post — it calls
/// `PreparePostForClientWithEmbedsAndImages` with empty opts, so `IncludePriority` is false —
/// and then fetches both for the whole page in one query each, keyed on `list.Order`. A port
/// that reused the single-post path would produce the same bytes here and a different number of
/// round trips; a port that keyed on the `posts` map instead of on `order` would attach priority
/// to parent posts Go leaves bare, which is what the second half of this test checks.
#[tokio::test]
async fn priority_and_acknowledgements_come_from_the_batch_reads() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/posts", fixture.channel);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "a page carrying priority and acknowledgements"
    );

    let list = decode(&go, "a page carrying priority");
    let metadata = &list["posts"][&fixture.root]["metadata"];
    assert_eq!(metadata["priority"]["priority"], "urgent");
    assert_eq!(metadata["priority"]["requested_ack"], true);
    assert_eq!(
        metadata["acknowledgements"][0]["acknowledged_at"], 1_700_000_000_123_i64,
        "the planted timestamp, not a CreateAt"
    );
    assert!(
        !list["posts"][&fixture.reacted]["metadata"]["reactions"]
            .as_array()
            .expect("reactions")
            .is_empty(),
        "and the reaction on the other post, which comes from the per-post path instead"
    );

    // The same post on a page it reaches only as a **parent**. Go batches priority over
    // `list.Order`, which does not name it there, so it must come back bare — and a port that
    // keyed the batch on the `posts` map instead would hand back the priority Go withholds.
    let narrow = format!(
        "/api/v4/channels/{}/posts?per_page=2&page=2",
        fixture.channel
    );
    let (go_narrow, rs_narrow) = fetch_both(&client, &token, &narrow).await;
    let narrow_list = decode(&go_narrow, "a narrow page");
    assert!(
        narrow_list["posts"][&fixture.root]["id"].is_string(),
        "the root is on this page as a parent"
    );
    assert!(
        narrow_list["posts"][&fixture.root]["metadata"]["priority"].is_null(),
        "and carries no priority there: {}",
        narrow_list["posts"][&fixture.root]["metadata"]
    );
    assert_eq!(
        String::from_utf8_lossy(&go_narrow),
        String::from_utf8_lossy(&rs_narrow),
        "a narrow page"
    );
}

/// `skipFetchThreads` is not a filter — it decides whether `reply_count` is computed at all.
#[tokio::test]
async fn skip_fetch_threads_is_byte_identical_and_moves_reply_count() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let off = format!("/api/v4/channels/{}/posts", fixture.channel);
    let on = format!(
        "/api/v4/channels/{}/posts?skipFetchThreads=true",
        fixture.channel
    );
    let (go_off, rs_off) = fetch_both(&client, &token, &off).await;
    let (go_on, rs_on) = fetch_both(&client, &token, &on).await;
    assert_eq!(
        String::from_utf8_lossy(&go_off),
        String::from_utf8_lossy(&rs_off),
        "skipFetchThreads off"
    );
    assert_eq!(
        String::from_utf8_lossy(&go_on),
        String::from_utf8_lossy(&rs_on),
        "skipFetchThreads on"
    );

    // The flag has to *do* something, or both comparisons above are the same comparison twice.
    assert_eq!(
        decode(&go_off, "off")["posts"][&fixture.root]["reply_count"],
        0,
        "with the flag off Go selects no ReplyCount column at all"
    );
    assert_eq!(
        decode(&go_on, "on")["posts"][&fixture.root]["reply_count"],
        2,
        "with it on the subquery counts the thread"
    );
}

/// A window that cuts a thread in half: the replies are in `order`, their root is only in `posts`.
#[tokio::test]
async fn a_page_carries_parents_that_are_not_in_its_order() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    // This window holds `reply b` and `root three`; the root of `reply b` is older than both, so
    // `getParentsPosts` is the only thing that can put it in the map.
    let path = format!(
        "/api/v4/channels/{}/posts?per_page=2&page=2",
        fixture.channel
    );
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "a mid-channel page"
    );

    let list = decode(&go, "a mid-channel page");
    let order = list["order"].as_array().expect("an order");
    let posts = list["posts"].as_object().expect("a posts map");
    assert_eq!(order.len(), 2, "the page size");
    assert!(
        posts.len() > order.len(),
        "the parents query must have contributed at least one post outside the order"
    );
    assert!(
        !order.iter().any(|id| id == &fixture.root) && posts.contains_key(&fixture.root),
        "the thread's root is in the map and not in the order"
    );
    assert!(
        !list["next_post_id"].as_str().unwrap_or_default().is_empty(),
        "a middle page has a newer post to point at"
    );
    assert!(
        !list["prev_post_id"].as_str().unwrap_or_default().is_empty(),
        "and an older one"
    );
}

/// The etag matches, answers a 304, and — the part that looks like a bug — ignores
/// `collapsedThreads`.
#[tokio::test]
async fn the_etag_matches_answers_304_and_ignores_collapsed_threads() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/posts", fixture.channel);
    let (go_etag, rs_etag) = etags(&client, &token, &path).await;
    assert_eq!(go_etag, rs_etag, "the etag");

    let collapsed = format!(
        "/api/v4/channels/{}/posts?collapsedThreads=true",
        fixture.channel
    );
    let (go_collapsed_etag, rs_collapsed_etag) = etags(&client, &token, &collapsed).await;
    assert_eq!(
        go_collapsed_etag, go_etag,
        "Go drops the RootId filter it means to apply (post_store.go:954), so both modes share \
         one etag — reproduced deliberately"
    );
    assert_eq!(rs_collapsed_etag, go_collapsed_etag, "and so do we");

    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("If-None-Match", &go_etag)
            .send()
            .await
            .expect("the server answers");
        assert_eq!(response.status(), 304, "{base}: a matching etag is a 304");
        assert_eq!(
            response.headers().get("etag").and_then(|v| v.to_str().ok()),
            Some(go_etag.as_str()),
            "{base}: the 304 carries the etag back"
        );
        assert!(
            response.bytes().await.expect("a body").is_empty(),
            "{base}: a 304 has no body"
        );
    }
}

/// An empty channel's etag is a clock reading — and Go **caches** it, which is where the two
/// servers part company.
///
/// `LocalCachePostStore.GetEtag` (localcachelayer/post_layer.go:74) memoises the channel's last
/// post time for thirty minutes and only drops it when a post is written through the store. For
/// a channel with no posts there is no last post time, so what gets cached is the reading of the
/// clock the *first* request happened to take, and every later request repeats it. This server
/// has no cache layer, so it stamps a fresh reading each time.
///
/// The body is identical either way; the header is not, and Go can answer a 304 to a client that
/// echoes the cached etag back where we would answer 200 with the same empty list. Recorded as
/// [D-159] rather than reproduced — a cache is not a translation.
#[tokio::test]
async fn an_empty_channel_stamps_the_etag_with_the_clock() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/posts", fixture.private_channel);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "an empty channel"
    );
    assert_eq!(
        decode(&go, "an empty channel")["order"],
        serde_json::json!([]),
        "the fixture channel really is empty, or this test is about something else"
    );

    // Ours is a reading of the clock taken during the request. Bracketing it is exact where
    // "the two differ" would flake whenever both landed in the same millisecond.
    let before = now_millis();
    let (go_first, rs_etag) = etags(&client, &token, &path).await;
    let after = now_millis();
    let stamp: i64 = rs_etag
        .rsplit_once('.')
        .and_then(|(_, millis)| millis.parse().ok())
        .unwrap_or_else(|| panic!("{rs_etag} does not end in a number"));
    assert!(
        (before..=after).contains(&stamp),
        "{rs_etag} is not a reading of the clock taken during the request"
    );

    // Go's is the same reading its first request took, however long ago that was.
    let (go_second, _) = etags(&client, &token, &path).await;
    assert_eq!(
        go_first, go_second,
        "Go's empty-channel etag is cached, so two reads agree — this is the divergence, not a \
         coincidence"
    );
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// The deleted post appears only for a caller who may ask for it.
#[tokio::test]
async fn include_deleted_matches_go_for_an_admin() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let path = format!(
        "/api/v4/channels/{}/posts?include_deleted=true",
        fixture.channel
    );
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "include_deleted for an admin"
    );

    let with = decode(&go, "with deleted");
    let order: Vec<&str> = with["order"]
        .as_array()
        .expect("an order")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        order.contains(&fixture.doomed.as_str()),
        "the soft-deleted post is on this page and on no other"
    );
    assert!(
        with["posts"][&fixture.doomed]["delete_at"]
            .as_i64()
            .unwrap_or_default()
            > 0,
        "and it really is deleted"
    );
    assert_eq!(
        with["posts"][&fixture.doomed]["message"], "",
        "the deleted-post short circuit empties the message"
    );
}

/// The `include_deleted` gate runs **before** the channel is fetched, so it is the error a caller
/// who could not read the channel either way still gets.
#[tokio::test]
async fn include_deleted_is_refused_for_a_plain_user_before_any_channel_check() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    // A channel this user is not in, so both gates would refuse — and only one of them can be
    // the one that answers.
    let path = format!(
        "/api/v4/channels/{}/posts?include_deleted=true",
        fixture.private_channel
    );
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &fixture.plain_token, &path).await;
    assert_eq!(go_status, 403, "Go refuses");
    assert_eq!(rs_status, go_status, "and so do we");

    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "include_deleted");
    assert_eq!(go["id"], "api.context.permissions.app_error");
    // **The permission id is not on the wire.** `MakePermissionError` puts it in
    // `DetailedError`, and `AppError`'s marshalling drops that for anyone who is not a system
    // admin — so this refusal is byte-identical to the channel one below, and which gate fired
    // is visible only in the server log. Asserting on the id here would be asserting on a value
    // no client can see.
    assert_eq!(go["detailed_error"], "", "stripped for a non-admin caller");
}

/// A private channel the caller is not in refuses with `read_channel_content` — there is no
/// second `read_public_channel` fallback here, unlike `getPost`.
#[tokio::test]
async fn a_channel_the_caller_cannot_read_is_refused() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/posts", fixture.private_channel);
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &fixture.plain_token, &path).await;
    assert_eq!(go_status, 403, "Go refuses");
    assert_eq!(rs_status, go_status, "and so do we");

    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a private channel");
    assert_eq!(go["id"], "api.context.permissions.app_error");
}

/// An open channel the caller is *not* a member of is still readable: the first fallback inside
/// `SessionHasPermissionToReadChannel` grants it at team scope.
#[tokio::test]
async fn an_open_channel_is_readable_by_a_team_member_who_never_joined() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    // `statuses` rather than `fetch_both_raw` because this channel's one post carries a link,
    // so our side forwards it — the claim under test is the *gate*, not who answered.
    let path = format!("/api/v4/channels/{}/posts", fixture.link_channel);
    let (go_status, rs_status) = statuses(&client, &fixture.plain_token, &path).await;
    assert_eq!(go_status, 200, "Go serves a non-member of an open channel");
    assert_eq!(rs_status, go_status, "and so do we");
}

/// Everything the port declines, and the proof that it declines rather than guesses.
#[tokio::test]
async fn the_shapes_this_port_declines_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;
    let channel = &fixture.channel;

    for query in [
        // The three cursor branches, each a different store query.
        "?since=1".to_owned(),
        format!("?after={}", fixture.root),
        format!("?before={}", fixture.newest),
        // Sanitized participant profiles.
        "?collapsedThreadsExtended=true".to_owned(),
        // A `since` that does not parse — Go's 400, with a detail string we cannot reproduce.
        "?since=not-a-number".to_owned(),
    ] {
        assert_forwarded_and_identical(
            &client,
            &token,
            &format!("/api/v4/channels/{channel}/posts{query}"),
        )
        .await;
    }

    // A message the markdown parser would find a link in refuses the *whole page*, because one
    // unreproducible post is enough to make the list unreproducible.
    assert_forwarded_and_identical(
        &client,
        &token,
        &format!("/api/v4/channels/{}/posts", fixture.link_channel),
    )
    .await;
}

/// `?since=0` is not a cursor: Go's test is `since > 0`, so it takes the page branch.
#[tokio::test]
async fn since_zero_is_served_as_a_plain_page() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/posts?since=0", fixture.channel);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "since=0"
    );

    // And an empty value is the same as an absent one, in Go and here.
    let empty = format!("/api/v4/channels/{}/posts?after=", fixture.channel);
    let (go, rs) = fetch_both(&client, &token, &empty).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "after="
    );
}

/// A channel id of the wrong shape is a 400 from `RequireChannelId`, not a 404.
#[tokio::test]
async fn an_invalid_channel_id_matches_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, "/api/v4/channels/tooshort/posts").await;
    assert_eq!(go_status, 400, "Go rejects the id");
    assert_eq!(rs_status, go_status, "and so do we");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a short id");
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}
