//! Cross-server parity for the two per-thread read-state writes:
//!
//! ```text
//! PUT  /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/read/{timestamp}
//! POST /api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/set_unread/{post_id}
//! ```
//!
//! ```sh
//! scripts/parity.sh --test parity thread_read
//! ```
//!
//! # The same thread, planted twice
//!
//! Unlike the twin-thread discipline of `thread_writes`, every comparison here runs the write on
//! **one** thread against both servers in turn, re-planting the `ThreadMemberships` row to the
//! same state in between. The response is the `ThreadResponse` for that thread, so with the
//! same prior row and the same timestamp the two bodies must be **byte-identical** — ids, counts
//! and `last_viewed_at` included — and so must the `thread_read_changed` event's data, since its
//! `previous_*` counters come from the re-planted row. The row each write leaves behind is
//! compared column by column, except `LastUpdated`, which is a clock.
//!
//! # What the mention scenarios pin
//!
//! `unread_mentions` is `countThreadMentions`: the mention engine over the **markdown text
//! nodes** of every reply since the timestamp. Each scenario is one reply set chosen so that a
//! plausible wrong port answers a different number — a raw-message scan counts the code-span
//! case, an `is_alphabetic` split changes the punctuation case, a case-insensitive first name
//! changes the name case, a `>` where Go has `>=` changes the timestamp case.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, RUST, SocketProbe, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel_typed,
    create_direct_channel, create_plain_user, create_team, delete_post, go_minted_token,
    logged_in_user_id, plain_username, purge_api_fixtures, stack_enabled,
};

/// Planted `LastViewed`: before any post this suite creates, so a timestamp of it or later is
/// always a forward move.
const PLANTED_VIEWED: i64 = 1_000_000;
const PLANTED_UPDATED: i64 = 2_000_000;
/// Non-zero, so `previous_unread_mentions` on the event is not the number a port that ignored
/// the row would produce.
const PLANTED_MENTIONS: i64 = 3;

struct Fixture {
    team_id: String,
    channel_id: String,
    /// A private channel the reader is not in.
    closed_id: String,
    /// The reader: first name `Zed` with first-name mentions on, mention keys `José,Boss`.
    reader: common::PlainUser,
    reader_name: String,
    /// A second plain user in the channel, author of some replies and third member of the GM.
    other: common::PlainUser,
    /// A DM between the admin and the reader.
    dm_id: String,
    /// A GM of the admin, the reader and `other`.
    gm_id: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "thrd").await;
            let channel_id = create_channel_typed(client, token, &team_id, "thrd", "O").await;
            let closed_id = create_channel_typed(client, token, &team_id, "thrdshut", "P").await;

            let reader = create_plain_user(client, token, &team_id, "thrd").await;
            let other = create_plain_user(client, token, &team_id, "thrdb").await;
            add_user_to_channel(client, token, &channel_id, &reader.id).await;
            add_user_to_channel(client, token, &channel_id, &other.id).await;

            // The reader's profile, through Go so its user cache learns of it. `notify_props`
            // on a patch replaces the whole map, so the defaults are restated.
            let response = client
                .put(format!("{GO}/api/v4/users/{}/patch", reader.id))
                .header("Authorization", format!("Bearer {token}"))
                .json(&serde_json::json!({
                    "first_name": "Zed",
                    "notify_props": {
                        "channel": "true",
                        "comments": "never",
                        "desktop": "mention",
                        "desktop_sound": "true",
                        "email": "true",
                        "first_name": "true",
                        "mention_keys": "José,Boss",
                        "push": "mention",
                        "push_status": "online",
                    }
                }))
                .send()
                .await
                .expect("Go answers");
            assert!(
                response.status().is_success(),
                "patching the reader failed: {}",
                response.text().await.unwrap_or_default()
            );

            let dm_id = create_direct_channel(client, token, logged_in_user_id(), &reader.id).await;

            let response = client
                .post(format!("{GO}/api/v4/channels/group"))
                .header("Authorization", format!("Bearer {token}"))
                .json(&serde_json::json!([
                    logged_in_user_id(),
                    reader.id,
                    other.id
                ]))
                .send()
                .await
                .expect("Go answers");
            assert!(
                response.status().is_success(),
                "creating the GM failed: {}",
                response.text().await.unwrap_or_default()
            );
            let gm: serde_json::Value = response.json().await.expect("a channel");
            let gm_id = gm["id"].as_str().expect("an id").to_owned();

            Fixture {
                team_id,
                channel_id,
                closed_id,
                reader_name: plain_username("thrd"),
                reader,
                other,
                dm_id,
                gm_id,
            }
        })
        .await
}

// ---------------------------------------------------------------------------------------------
// posts and rows
// ---------------------------------------------------------------------------------------------

/// A post through Go, returning its id and `create_at`. Posts are separated by a couple of
/// milliseconds so no two in a thread share a `create_at` — the timestamp scenarios need a
/// strict order.
async fn post(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    message: &str,
    root_id: Option<&str>,
) -> (String, i64) {
    tokio::time::sleep(Duration::from_millis(3)).await;
    let response = client
        .post(format!("{GO}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel_id,
            "message": message,
            "root_id": root_id.unwrap_or_default(),
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "posting {message:?} failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the post decodes");
    (
        created["id"].as_str().expect("an id").to_owned(),
        created["create_at"].as_i64().expect("create_at"),
    )
}

/// A root by the admin plus replies `(token, message)` in order; returns the root and the
/// replies' `(id, create_at)`.
async fn thread(
    client: &reqwest::Client,
    admin: &str,
    channel_id: &str,
    replies: &[(&str, &str)],
) -> ((String, i64), Vec<(String, i64)>) {
    let root = post(client, admin, channel_id, "root", None).await;
    let mut out = Vec::new();
    for (token, message) in replies {
        out.push(post(client, token, channel_id, message, Some(&root.0)).await);
    }
    (root, out)
}

async fn test_pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set for this suite");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the fixture database is reachable")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Membership {
    following: bool,
    last_viewed: i64,
    last_updated: i64,
    unread_mentions: i64,
}

fn planted() -> Membership {
    Membership {
        following: true,
        last_viewed: PLANTED_VIEWED,
        last_updated: PLANTED_UPDATED,
        unread_mentions: PLANTED_MENTIONS,
    }
}

async fn plant_membership(user_id: &str, post_id: &str, state: Membership) {
    let pool = test_pool().await;
    sqlx::query(
        "INSERT INTO threadmemberships (postid, userid, following, lastviewed, lastupdated, unreadmentions) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (postid, userid) DO UPDATE \
            SET following = $3, lastviewed = $4, lastupdated = $5, unreadmentions = $6",
    )
    .bind(post_id)
    .bind(user_id)
    .bind(state.following)
    .bind(state.last_viewed)
    .bind(state.last_updated)
    .bind(state.unread_mentions)
    .execute(&pool)
    .await
    .expect("the membership plants");
}

async fn drop_membership(user_id: &str, post_id: &str) {
    let pool = test_pool().await;
    sqlx::query("DELETE FROM threadmemberships WHERE userid = $1 AND postid = $2")
        .bind(user_id)
        .bind(post_id)
        .execute(&pool)
        .await
        .expect("the membership clears");
}

async fn read_membership(user_id: &str, post_id: &str) -> Option<Membership> {
    let pool = test_pool().await;
    let row: Option<(bool, i64, i64, i64)> = sqlx::query_as(
        "SELECT COALESCE(following, FALSE), COALESCE(lastviewed, 0), COALESCE(lastupdated, 0), \
                COALESCE(unreadmentions, 0) \
           FROM threadmemberships WHERE userid = $1 AND postid = $2",
    )
    .bind(user_id)
    .bind(post_id)
    .fetch_optional(&pool)
    .await
    .expect("the membership reads");
    row.map(
        |(following, last_viewed, last_updated, unread_mentions)| Membership {
            following,
            last_viewed,
            last_updated,
            unread_mentions,
        },
    )
}

// ---------------------------------------------------------------------------------------------
// requests
// ---------------------------------------------------------------------------------------------

fn read_path(user_id: &str, team_id: &str, thread_id: &str, timestamp: &str) -> String {
    format!("/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/read/{timestamp}")
}

fn set_unread_path(user_id: &str, team_id: &str, thread_id: &str, post_id: &str) -> String {
    format!("/api/v4/users/{user_id}/teams/{team_id}/threads/{thread_id}/set_unread/{post_id}")
}

async fn send(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    token: &str,
    path: &str,
) -> (u16, Vec<u8>, Option<String>) {
    let response = client
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("body").to_vec(),
        served_by,
    )
}

/// The `thread_read_changed` frames for this thread.
fn read_changed_for(probe: &SocketProbe, thread_id: &str) -> Vec<serde_json::Value> {
    probe
        .events_named("thread_read_changed")
        .into_iter()
        .filter(|f| f["data"]["thread_id"] == thread_id)
        .collect()
}

/// What one write left behind on one server.
struct Outcome {
    status: u16,
    body: Vec<u8>,
    row: Option<Membership>,
    read_events: Vec<serde_json::Value>,
    /// Every frame, for the set-unread ordering assertion.
    frames: Vec<serde_json::Value>,
}

/// Run `method path` as the reader against `base`, from a freshly planted (or dropped) row,
/// with the reader's socket open on that server.
async fn run_on(
    client: &reqwest::Client,
    f: &Fixture,
    base: &str,
    method: reqwest::Method,
    thread_id: &str,
    prior: Option<Membership>,
    path: &str,
) -> Outcome {
    match prior {
        Some(state) => plant_membership(&f.reader.id, thread_id, state).await,
        None => drop_membership(&f.reader.id, thread_id).await,
    }
    let mut socket = SocketProbe::connect(base, &f.reader.token).await;
    let (status, body, served) = send(client, base, method, &f.reader.token, path).await;
    if base == RUST {
        assert_eq!(served.as_deref(), Some("rust"), "{path} was forwarded");
    }
    if status == 200 {
        let thread_id = thread_id.to_owned();
        assert!(
            socket
                .collect_until(Duration::from_millis(2500), |frames| {
                    frames.iter().any(|f| {
                        f["event"] == "thread_read_changed" && f["data"]["thread_id"] == thread_id
                    })
                })
                .await,
            "{base} published no thread_read_changed for {thread_id}: {:?}",
            socket.raw
        );
    } else {
        socket.collect_for(Duration::from_millis(300)).await;
    }
    Outcome {
        status,
        body,
        row: read_membership(&f.reader.id, thread_id).await,
        read_events: read_changed_for(&socket, thread_id),
        frames: socket.frames(),
    }
}

/// Go then us, same thread, same prior row; assert the two agree on everything the wire and
/// the database can show, and hand back the parsed body.
async fn compare(
    client: &reqwest::Client,
    f: &Fixture,
    method: reqwest::Method,
    thread_id: &str,
    prior: Option<Membership>,
    path: &str,
) -> serde_json::Value {
    let _broadcast = BROADCAST_STREAM.lock().await;
    let go = run_on(client, f, GO, method.clone(), thread_id, prior, path).await;
    let rs = run_on(client, f, RUST, method.clone(), thread_id, prior, path).await;

    assert_eq!(
        go.status,
        200,
        "Go refused {path}: {}",
        String::from_utf8_lossy(&go.body)
    );
    assert_eq!(
        rs.status,
        200,
        "we refused {path}: {}",
        String::from_utf8_lossy(&rs.body)
    );
    assert_eq!(
        go.body,
        rs.body,
        "the bodies differ:\n go: {}\n us: {}",
        String::from_utf8_lossy(&go.body),
        String::from_utf8_lossy(&rs.body)
    );
    assert!(go.body.ends_with(b"\n"), "json.Encoder's newline");

    let go_row = go.row.expect("Go left a row");
    let rs_row = rs.row.expect("we left a row");
    assert_eq!(go_row.following, rs_row.following, "Following differs");
    assert_eq!(go_row.last_viewed, rs_row.last_viewed, "LastViewed differs");
    assert_eq!(
        go_row.unread_mentions, rs_row.unread_mentions,
        "UnreadMentions differs"
    );
    assert!(
        go_row.last_updated > PLANTED_UPDATED && rs_row.last_updated > PLANTED_UPDATED,
        "LastUpdated moved to the clock on both: go {} us {}",
        go_row.last_updated,
        rs_row.last_updated
    );

    assert_eq!(
        go.read_events.len(),
        1,
        "Go published one: {:?}",
        go.read_events
    );
    assert_eq!(
        rs.read_events.len(),
        1,
        "we published one: {:?}",
        rs.read_events
    );
    assert_eq!(
        go.read_events[0]["data"], rs.read_events[0]["data"],
        "the event data differs"
    );
    assert_eq!(
        go.read_events[0]["broadcast"], rs.read_events[0]["broadcast"],
        "the event addressing differs"
    );

    let body: serde_json::Value = serde_json::from_slice(&rs.body).expect("JSON");
    // The body and the row agree with each other, and the event with both.
    assert_eq!(body["unread_mentions"], rs_row.unread_mentions);
    assert_eq!(body["last_viewed_at"], rs_row.last_viewed);
    assert_eq!(
        rs.read_events[0]["data"]["unread_mentions"],
        body["unread_mentions"]
    );
    assert_eq!(
        rs.read_events[0]["data"]["unread_replies"],
        body["unread_replies"]
    );
    assert_eq!(rs.read_events[0]["data"]["timestamp"], rs_row.last_viewed);
    // `previous_unread_mentions` is the row as the read-state write found it: the planted value
    // on `/read/{timestamp}`, and **0** on `set_unread`, whose follow zeroed the column first.
    let expected_previous = match (&method, prior) {
        (&reqwest::Method::PUT, Some(prior)) => prior.unread_mentions,
        _ => 0,
    };
    assert_eq!(
        rs.read_events[0]["data"]["previous_unread_mentions"],
        expected_previous
    );
    assert_eq!(
        rs.read_events[0]["broadcast"]["team_id"],
        f.team_id.as_str()
    );
    assert_eq!(
        rs.read_events[0]["broadcast"]["user_id"],
        f.reader.id.as_str()
    );
    assert_eq!(rs.read_events[0]["broadcast"]["channel_id"], "");

    let _ = (&go.frames, &rs.frames);
    body
}

async fn compare_read(
    client: &reqwest::Client,
    f: &Fixture,
    thread_id: &str,
    timestamp: i64,
) -> serde_json::Value {
    compare_read_from(client, f, thread_id, timestamp, planted()).await
}

/// [`compare_read`] from a chosen prior row — for the cases where the row the write *finds*
/// has to sit on a boundary, since `previous_unread_replies` is counted from it.
async fn compare_read_from(
    client: &reqwest::Client,
    f: &Fixture,
    thread_id: &str,
    timestamp: i64,
    prior: Membership,
) -> serde_json::Value {
    let path = read_path(&f.reader.id, &f.team_id, thread_id, &timestamp.to_string());
    compare(
        client,
        f,
        reqwest::Method::PUT,
        thread_id,
        Some(prior),
        &path,
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// PUT …/read/{timestamp}: the mention count
// ---------------------------------------------------------------------------------------------

/// Two of three replies mention the reader, once in upper case; the third does not.
#[tokio::test]
async fn a_plain_mention_counts_case_insensitively_and_the_bodies_are_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let at = format!("@{}", f.reader_name);
    let (root, _) = thread(
        &client,
        &token,
        &f.channel_id,
        &[
            (&token, &format!("hello {at}, welcome")),
            (&token, "nothing here"),
            (&token, &format!("{}!", at.to_uppercase())),
        ],
    )
    .await;

    let body = compare_read(&client, f, &root.0, root.1).await;
    assert_eq!(body["unread_mentions"], 2);
    assert_eq!(body["unread_replies"], 3);
    assert_eq!(body["last_viewed_at"], root.1);
    assert_eq!(body["reply_count"], 3);
}

/// The parser sees markdown **text nodes**: a mention in a code span, a fenced block or a link
/// destination is invisible; one in link text or a block quote is not.
#[tokio::test]
async fn markdown_hides_mentions_in_code_and_link_destinations() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let at = format!("@{}", f.reader_name);
    let (root, _) = thread(
        &client,
        &token,
        &f.channel_id,
        &[
            (&token, &format!("`{at}`")),
            (&token, &format!("```\n{at}\n```")),
            (&token, &format!("    {at} indented code")),
            (&token, &format!("[{at}](http://example.com)")),
            (&token, &format!("[link](http://{at}.example.com)")),
            (&token, &format!("> {at}")),
            (&token, &format!("- {at}\n- nobody")),
            (&token, &format!("**{at}** in bold, which is text")),
        ],
    )
    .await;

    let body = compare_read(&client, f, &root.0, root.1).await;
    assert_eq!(
        body["unread_mentions"], 4,
        "link text, quote, list item, emphasis"
    );
    assert_eq!(body["unread_replies"], 8);
}

/// `@channel`, `@all` and `@here` all count for a user whose `channel` notify prop is on — the
/// reader is assumed online — and `@everyone` is nothing.
#[tokio::test]
async fn system_mentions_count_when_the_reader_allows_them() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (root, _) = thread(
        &client,
        &token,
        &f.channel_id,
        &[
            (&token, "@channel look"),
            (&token, "@all look"),
            (&token, "@here look"),
            (&token, "@everyone look"),
            (&token, "@Channel look"),
        ],
    )
    .await;

    let body = compare_read(&client, f, &root.0, root.1).await;
    assert_eq!(body["unread_mentions"], 4);
}

/// The word splitter and the suffix rules: trailing punctuation is peeled, an emoji shape is
/// skipped, a glued prefix is a different word.
#[tokio::test]
async fn punctuation_and_emoji_shapes_around_a_mention() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let at = format!("@{}", f.reader_name);
    let (root, _) = thread(
        &client,
        &token,
        &f.channel_id,
        &[
            (&token, &format!("{at}.")),
            (&token, &format!("({at})")),
            (&token, &format!(":{at}:")),
            (&token, &format!("x{at}")),
            (&token, &format!("{at}_")),
            (&token, &format!("{at}s")),
            (&token, &format!("cc:{at}.done")),
            (&token, &format!("«{at}»")),
        ],
    )
    .await;

    let body = compare_read(&client, f, &root.0, root.1).await;
    assert_eq!(body["unread_mentions"], 5, "., (), _, cc:, «»");
}

/// The first name matches case-sensitively; mention keys and the username do not; a
/// multibyte mention key matches as a substring of a multibyte word.
#[tokio::test]
async fn first_name_is_case_sensitive_and_mention_keys_are_not() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (root, _) = thread(
        &client,
        &token,
        &f.channel_id,
        &[
            (&token, "Zed, hi"),
            (&token, "zed, hi"),
            (&token, "the boss is in"),
            (&token, "BOSS"),
            (&token, "josé is here"),
            (&token, "JOSÉ is here"),
            (&token, "xxjoséxx"),
            (&token, "Zeddy"),
        ],
    )
    .await;

    let body = compare_read(&client, f, &root.0, root.1).await;
    assert_eq!(
        body["unread_mentions"], 6,
        "Zed, boss, BOSS, josé, JOSÉ, xxjoséxx"
    );
}

/// Mentions are counted from `CreateAt >= timestamp`; unread replies from `CreateAt >
/// LastViewed`. A timestamp equal to a reply's `create_at` is on different sides of the two.
#[tokio::test]
async fn the_timestamp_is_inclusive_for_mentions_and_strict_for_unread_replies() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let at = format!("@{}", f.reader_name);
    let (root, replies) = thread(
        &client,
        &token,
        &f.channel_id,
        &[(&token, &at), (&token, &at), (&token, &at)],
    )
    .await;
    assert!(replies[0].1 < replies[1].1 && replies[1].1 < replies[2].1);

    let body = compare_read(&client, f, &root.0, replies[1].1).await;
    assert_eq!(body["unread_mentions"], 2, "the second and third");
    assert_eq!(body["unread_replies"], 1, "only the third");
    assert_eq!(body["last_viewed_at"], replies[1].1);

    // One past the last reply: everything read.
    let body = compare_read(&client, f, &root.0, replies[2].1 + 1).await;
    assert_eq!(body["unread_mentions"], 0);
    assert_eq!(body["unread_replies"], 0);

    // The *previous* unread-reply count on the event is `GetThreadUnreadReplyCount` over the row
    // the write found, and that count is strict too: a prior `LastViewed` equal to the second
    // reply's `create_at` sees only the third as unread. The planted row everywhere else in
    // this file sits far below every post, where `>` and `>=` agree — measured: the `>=`
    // mutation survived until this case existed. `compare` already asserts the event data
    // against Go's; the explicit number pins which side of the boundary the row is on.
    let prior = Membership {
        last_viewed: replies[1].1,
        ..planted()
    };
    let _broadcast = BROADCAST_STREAM.lock().await;
    let rs = run_on(
        &client,
        f,
        RUST,
        reqwest::Method::PUT,
        &root.0,
        Some(prior),
        &read_path(
            &f.reader.id,
            &f.team_id,
            &root.0,
            &(replies[2].1 + 1).to_string(),
        ),
    )
    .await;
    drop(_broadcast);
    assert_eq!(rs.status, 200);
    assert_eq!(
        rs.read_events[0]["data"]["previous_unread_replies"], 1,
        "strictly after the prior mark"
    );
    assert_eq!(rs.read_events[0]["data"]["unread_replies"], 0);
    let _ = compare_read_from(&client, f, &root.0, replies[2].1 + 1, prior).await;
}

/// A deleted reply is neither unread nor a mention; the reader's own reply counts like anyone
/// else's.
#[tokio::test]
async fn deleted_replies_are_skipped_and_own_replies_count() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let at = format!("@{}", f.reader_name);
    let (root, replies) = thread(
        &client,
        &token,
        &f.channel_id,
        &[
            (&token, &at),
            (&f.reader.token, &format!("{at} note to self")),
            (&f.other.token, &at),
        ],
    )
    .await;
    delete_post(&client, &token, &replies[0].0).await;

    let body = compare_read(&client, f, &root.0, root.1).await;
    assert_eq!(body["unread_mentions"], 2);
    assert_eq!(body["unread_replies"], 2);
}

/// In a DM every reply by the other side is a mention and the reader's own are not.
#[tokio::test]
async fn in_a_direct_message_every_reply_by_the_other_side_is_a_mention() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (root, _) = thread(
        &client,
        &token,
        &f.dm_id,
        &[
            (&token, "no at-mention at all"),
            (&f.reader.token, "mine"),
            (&token, "`code` still counts here"),
        ],
    )
    .await;

    let body = compare_read(&client, f, &root.0, root.1).await;
    assert_eq!(body["unread_mentions"], 2);
    assert_eq!(body["unread_replies"], 3);
}

/// In a group message nothing is a mention: `GetOtherUserIdForDM` answers `""` for a channel
/// whose name is not `id__id`, and no reply is authored by `""`.
#[tokio::test]
async fn in_a_group_message_nothing_is_a_mention() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let at = format!("@{}", f.reader_name);
    let (root, _) = thread(
        &client,
        &token,
        &f.gm_id,
        &[(&token, &at), (&f.other.token, &at), (&token, "@channel")],
    )
    .await;

    let body = compare_read(&client, f, &root.0, root.1).await;
    assert_eq!(body["unread_mentions"], 0);
    assert_eq!(body["unread_replies"], 3);
}

// ---------------------------------------------------------------------------------------------
// POST …/set_unread/{post_id}
// ---------------------------------------------------------------------------------------------

/// The mark moves to one millisecond before the post, so that post and everything after it is
/// unread; the follow that runs first creates the membership when there is none, and publishes
/// `thread_follow_changed` **before** `thread_read_changed`.
#[tokio::test]
async fn set_unread_moves_the_mark_to_just_before_the_post_and_follows_first() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let at = format!("@{}", f.reader_name);
    let (root, replies) = thread(
        &client,
        &token,
        &f.channel_id,
        &[(&token, &at), (&token, &at), (&token, "plain")],
    )
    .await;

    let path = set_unread_path(&f.reader.id, &f.team_id, &root.0, &replies[1].0);
    let _broadcast = BROADCAST_STREAM.lock().await;
    let go = run_on(&client, f, GO, reqwest::Method::POST, &root.0, None, &path).await;
    let rs = run_on(
        &client,
        f,
        RUST,
        reqwest::Method::POST,
        &root.0,
        None,
        &path,
    )
    .await;
    drop(_broadcast);

    assert_eq!(go.status, 200, "{}", String::from_utf8_lossy(&go.body));
    assert_eq!(rs.status, 200, "{}", String::from_utf8_lossy(&rs.body));
    assert_eq!(
        go.body,
        rs.body,
        "\n go: {}\n us: {}",
        String::from_utf8_lossy(&go.body),
        String::from_utf8_lossy(&rs.body)
    );
    let body: serde_json::Value = serde_json::from_slice(&rs.body).expect("JSON");
    assert_eq!(body["last_viewed_at"], replies[1].1 - 1);
    assert_eq!(body["unread_replies"], 2);
    assert_eq!(body["unread_mentions"], 1);

    let go_row = go.row.expect("Go created the row");
    let rs_row = rs.row.expect("we created the row");
    assert!(go_row.following && rs_row.following);
    assert_eq!(go_row.last_viewed, replies[1].1 - 1);
    assert_eq!(rs_row.last_viewed, replies[1].1 - 1);
    assert_eq!(go_row.unread_mentions, 1);
    assert_eq!(rs_row.unread_mentions, 1);

    let order = |frames: &[serde_json::Value]| -> Vec<String> {
        frames
            .iter()
            .filter(|f| {
                (f["event"] == "thread_follow_changed" || f["event"] == "thread_read_changed")
                    && f["data"]["thread_id"] == root.0.as_str()
            })
            .map(|f| f["event"].as_str().unwrap_or("").to_owned())
            .collect()
    };
    assert_eq!(
        order(&go.frames),
        vec!["thread_follow_changed", "thread_read_changed"],
        "Go: {:?}",
        go.frames
    );
    assert_eq!(
        order(&rs.frames),
        vec!["thread_follow_changed", "thread_read_changed"],
        "us: {:?}",
        rs.frames
    );
    assert_eq!(go.read_events[0]["data"], rs.read_events[0]["data"]);
    // The follow zeroed the mentions before the read-state write counted them again, so the
    // "previous" count on the event is 0 on both — the row that existed for a moment.
    assert_eq!(rs.read_events[0]["data"]["previous_unread_mentions"], 0);
}

/// The root post itself is a valid target: the mark goes to just before the root, so every
/// reply is unread.
#[tokio::test]
async fn set_unread_with_the_root_marks_everything_unread() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let at = format!("@{}", f.reader_name);
    let (root, _) = thread(
        &client,
        &token,
        &f.channel_id,
        &[(&token, &at), (&token, "x")],
    )
    .await;

    let path = set_unread_path(&f.reader.id, &f.team_id, &root.0, &root.0);
    let body = compare(
        &client,
        f,
        reqwest::Method::POST,
        &root.0,
        Some(planted()),
        &path,
    )
    .await;
    assert_eq!(body["last_viewed_at"], root.1 - 1);
    assert_eq!(body["unread_replies"], 2);
    assert_eq!(body["unread_mentions"], 1);
}

/// A post from a different thread is a 400 — and the follow has already run, so the row was
/// written before the refusal, on both.
#[tokio::test]
async fn set_unread_with_a_post_from_another_thread_is_a_400_after_the_follow() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (root, _) = thread(&client, &token, &f.channel_id, &[(&token, "x")]).await;
    let (stranger, _) = thread(&client, &token, &f.channel_id, &[(&token, "y")]).await;

    let path = set_unread_path(&f.reader.id, &f.team_id, &root.0, &stranger.0);
    drop_membership(&f.reader.id, &root.0).await;
    let (go_status, go_body, _) =
        send(&client, GO, reqwest::Method::POST, &f.reader.token, &path).await;
    let go_row = read_membership(&f.reader.id, &root.0).await;
    drop_membership(&f.reader.id, &root.0).await;
    let (rs_status, rs_body, served) =
        send(&client, RUST, reqwest::Method::POST, &f.reader.token, &path).await;
    let rs_row = read_membership(&f.reader.id, &root.0).await;

    assert_eq!(go_status, 400, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 400, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(served.as_deref(), Some("rust"));
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "other thread");
    assert_eq!(
        parsed["id"],
        "app.user.update_thread_read_for_user_by_post.app_error"
    );
    assert!(
        go_row.is_some_and(|r| r.following),
        "Go followed before refusing"
    );
    assert!(
        rs_row.is_some_and(|r| r.following),
        "we followed before refusing"
    );

    // An unknown post id is the post store's 404 instead, from the same `GetSinglePost`.
    let path = set_unread_path(
        &f.reader.id,
        &f.team_id,
        &root.0,
        "zzzzzzzzzzzzzzzzzzzzzzzzzz",
    );
    let (go_status, go_body, _) =
        send(&client, GO, reqwest::Method::POST, &f.reader.token, &path).await;
    let (rs_status, rs_body, _) =
        send(&client, RUST, reqwest::Method::POST, &f.reader.token, &path).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, 404);
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "unknown post");
    assert_eq!(parsed["id"], "app.post.get.app_error");
}

// ---------------------------------------------------------------------------------------------
// refusals
// ---------------------------------------------------------------------------------------------

/// `/read/{timestamp}` without a membership is the membership 404 — the route does not create
/// one the way `set_unread` does.
#[tokio::test]
async fn reading_a_thread_without_a_membership_is_a_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (root, _) = thread(&client, &token, &f.channel_id, &[(&token, "x")]).await;
    drop_membership(&f.reader.id, &root.0).await;

    let path = read_path(&f.reader.id, &f.team_id, &root.0, &root.1.to_string());
    let (go_status, go_body, _) =
        send(&client, GO, reqwest::Method::PUT, &f.reader.token, &path).await;
    let (rs_status, rs_body, served) =
        send(&client, RUST, reqwest::Method::PUT, &f.reader.token, &path).await;
    assert_eq!(go_status, 404, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 404, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(served.as_deref(), Some("rust"));
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "no membership");
    assert_eq!(
        parsed["id"],
        "app.user.get_thread_membership_for_user.not_found"
    );
    assert!(
        read_membership(&f.reader.id, &root.0).await.is_none(),
        "neither server created a row"
    );
}

/// An **unfollowed** membership: the two writes happen — `UpdateMembership` and `MarkAsRead`
/// run before `GetThreadForUser` — and then the read-back refuses the unfollowed thread with
/// `app.user.get_threads_for_user.not_found`. So the route answers 404 and still moved the row,
/// on both. A port that checked `Following` first, or that flipped it on, would differ.
#[tokio::test]
async fn reading_an_unfollowed_thread_writes_the_row_and_then_answers_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let at = format!("@{}", f.reader_name);
    let (root, replies) = thread(&client, &token, &f.channel_id, &[(&token, &at)]).await;
    let unfollowed = Membership {
        following: false,
        ..planted()
    };
    let path = read_path(&f.reader.id, &f.team_id, &root.0, &replies[0].1.to_string());

    plant_membership(&f.reader.id, &root.0, unfollowed).await;
    let (go_status, go_body, _) =
        send(&client, GO, reqwest::Method::PUT, &f.reader.token, &path).await;
    let go_row = read_membership(&f.reader.id, &root.0)
        .await
        .expect("Go kept the row");
    plant_membership(&f.reader.id, &root.0, unfollowed).await;
    let (rs_status, rs_body, served) =
        send(&client, RUST, reqwest::Method::PUT, &f.reader.token, &path).await;
    let rs_row = read_membership(&f.reader.id, &root.0)
        .await
        .expect("we kept the row");

    assert_eq!(go_status, 404, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 404, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(served.as_deref(), Some("rust"));
    let parsed = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "unfollowed");
    assert_eq!(parsed["id"], "app.user.get_threads_for_user.not_found");

    for (who, row) in [("Go", go_row), ("we", rs_row)] {
        assert!(!row.following, "{who}: still unfollowed");
        assert_eq!(
            row.last_viewed, replies[0].1,
            "{who}: the mark moved before the refusal"
        );
        assert_eq!(
            row.unread_mentions, 1,
            "{who}: the count was written before the refusal"
        );
        assert!(
            row.last_updated > PLANTED_UPDATED,
            "{who}: LastUpdated moved"
        );
    }
}

/// The validators, the two gates and the mux class, each compared.
#[tokio::test]
async fn the_refusals_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let (root, _) = thread(&client, &token, &f.channel_id, &[(&token, "x")]).await;
    let (closed_root, _) = thread(&client, &token, &f.closed_id, &[(&token, "x")]).await;
    plant_membership(&f.reader.id, &root.0, planted()).await;
    let ts = root.1.to_string();
    let good = "aaaaaaaaaaaaaaaaaaaaaaaaaa";

    // (label, method, token, path, status, id, served-by-us)
    let cases: Vec<(&str, reqwest::Method, &str, String, u16, &str, bool)> = vec![
        (
            "bad user id",
            reqwest::Method::PUT,
            &f.reader.token,
            read_path("short", &f.team_id, &root.0, &ts),
            400,
            "api.context.invalid_url_param.app_error",
            true,
        ),
        (
            "bad thread id",
            reqwest::Method::PUT,
            &f.reader.token,
            read_path(&f.reader.id, &f.team_id, "short", &ts),
            400,
            "api.context.invalid_url_param.app_error",
            true,
        ),
        (
            "bad team id",
            reqwest::Method::PUT,
            &f.reader.token,
            read_path(&f.reader.id, "short", &root.0, &ts),
            400,
            "api.context.invalid_url_param.app_error",
            true,
        ),
        (
            "zero timestamp",
            reqwest::Method::PUT,
            &f.reader.token,
            read_path(&f.reader.id, &f.team_id, &root.0, "0"),
            400,
            "api.context.invalid_url_param.app_error",
            true,
        ),
        (
            "overflowing timestamp",
            reqwest::Method::PUT,
            &f.reader.token,
            read_path(&f.reader.id, &f.team_id, &root.0, "99999999999999999999"),
            400,
            "api.context.invalid_url_param.app_error",
            true,
        ),
        (
            "non-digit timestamp is Go's mux 404",
            reqwest::Method::PUT,
            &f.reader.token,
            read_path(&f.reader.id, &f.team_id, &root.0, "now"),
            404,
            "api.context.404.app_error",
            false,
        ),
        (
            "negative timestamp is Go's mux 404",
            reqwest::Method::PUT,
            &f.reader.token,
            read_path(&f.reader.id, &f.team_id, &root.0, "-1"),
            404,
            "api.context.404.app_error",
            false,
        ),
        (
            "someone else's read state",
            reqwest::Method::PUT,
            &f.reader.token,
            read_path(&f.other.id, &f.team_id, &root.0, &ts),
            403,
            "api.context.permissions.app_error",
            true,
        ),
        (
            "a thread the reader cannot see",
            reqwest::Method::PUT,
            &f.reader.token,
            read_path(&f.reader.id, &f.team_id, &closed_root.0, &ts),
            403,
            "api.context.permissions.app_error",
            true,
        ),
        (
            "an unknown thread, as a plain user",
            reqwest::Method::PUT,
            &f.reader.token,
            read_path(&f.reader.id, &f.team_id, good, &ts),
            403,
            "api.context.permissions.app_error",
            true,
        ),
        (
            "an unknown thread, as the admin",
            reqwest::Method::PUT,
            &token,
            read_path(logged_in_user_id(), &f.team_id, good, &ts),
            404,
            "app.user.get_thread_membership_for_user.not_found",
            true,
        ),
        (
            "set_unread: bad post id",
            reqwest::Method::POST,
            &f.reader.token,
            set_unread_path(&f.reader.id, &f.team_id, &root.0, "short"),
            400,
            "api.context.invalid_url_param.app_error",
            true,
        ),
        (
            "set_unread: post id outside the mux class",
            reqwest::Method::POST,
            &f.reader.token,
            set_unread_path(&f.reader.id, &f.team_id, &root.0, "not-an-id"),
            404,
            "api.context.404.app_error",
            false,
        ),
        (
            "set_unread: someone else's",
            reqwest::Method::POST,
            &f.reader.token,
            set_unread_path(&f.other.id, &f.team_id, &root.0, &root.0),
            403,
            "api.context.permissions.app_error",
            true,
        ),
        (
            "set_unread: a thread the reader cannot see",
            reqwest::Method::POST,
            &f.reader.token,
            set_unread_path(&f.reader.id, &f.team_id, &closed_root.0, &closed_root.0),
            403,
            "api.context.permissions.app_error",
            true,
        ),
    ];

    for (label, method, tok, path, status, id, ours) in cases {
        let (go_status, go_body, _) = send(&client, GO, method.clone(), tok, &path).await;
        let (rs_status, rs_body, served) = send(&client, RUST, method, tok, &path).await;
        assert_eq!(
            go_status,
            status,
            "{label}: Go {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            status,
            "{label}: us {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_eq!(
            served.as_deref(),
            if ours { Some("rust") } else { Some("go") },
            "{label}: served by"
        );
        let parsed = if ours {
            assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label)
        } else {
            assert_forwarded_bodies_match(&go_body, &rs_body, label)
        };
        assert_eq!(parsed["id"], id, "{label}");
    }

    // Nothing above moved the planted row.
    assert_eq!(
        read_membership(&f.reader.id, &root.0).await,
        Some(planted()),
        "a refusal wrote the row"
    );
}

/// `me` resolves to the session user on both routes.
#[tokio::test]
async fn the_me_alias_resolves_to_the_session_user() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let at = format!("@{}", f.reader_name);
    let (root, replies) = thread(&client, &token, &f.channel_id, &[(&token, &at)]).await;

    let path = read_path("me", &f.team_id, &root.0, &root.1.to_string());
    let body = compare(
        &client,
        f,
        reqwest::Method::PUT,
        &root.0,
        Some(planted()),
        &path,
    )
    .await;
    assert_eq!(body["unread_mentions"], 1);

    let path = set_unread_path("me", &f.team_id, &root.0, &replies[0].0);
    let body = compare(
        &client,
        f,
        reqwest::Method::POST,
        &root.0,
        Some(planted()),
        &path,
    )
    .await;
    assert_eq!(body["last_viewed_at"], replies[0].1 - 1);
}

/// A forwarded answer is Go's own body on both sides, translated message included, so the
/// D-092 exemption `assert_error_bodies_match_except_known_gaps` applies does not fit: compare
/// the two as values with only `request_id` removed.
fn assert_forwarded_bodies_match(
    go_body: &[u8],
    rs_body: &[u8],
    context: &str,
) -> serde_json::Value {
    let mut go: serde_json::Value = serde_json::from_slice(go_body).expect("Go's body is JSON");
    let mut rs: serde_json::Value =
        serde_json::from_slice(rs_body).expect("the forwarded body is JSON");
    for body in [&mut go, &mut rs] {
        if let Some(object) = body.as_object_mut() {
            object.remove("request_id");
        }
    }
    assert_eq!(go, rs, "{context}: the forwarded body is not Go's");
    rs
}
