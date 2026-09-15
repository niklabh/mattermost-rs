//! Cross-server parity for `POST /api/v4/teams/{team_id}/files/search` and
//! `POST /api/v4/files/search` — `searchFilesInTeam` and `searchFilesInAllTeams`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity file_search
//! ```
//!
//! # The fixture is built so that a wrong answer cannot look like the right one
//!
//! Every file name carries the token `mmrsfs`, so an AND search can be pinned to this suite's
//! rows on a shared database, and each file carries one word of its own — `alpha`, `bravo`,
//! `charlie` — so a result set names the rows in it.
//!
//! - **The plain user is a member of two public channels, not of a third public one, and not of
//!   the private one.** The third separates the store's `ChannelMembers` join from the app's
//!   `FilterFilesByChannelPermissions`: a team member may *read* a public channel, so only the
//!   join keeps its files out.
//! - **One channel is archived after its file is posted**, so `include_deleted_channels` is a
//!   filter. One file is soft-deleted straight in `FileInfo`, for `DeleteAt = 0`.
//! - **One file is uploaded and never attached to a post**, for `PostId <> ''`; one attached
//!   file's post is planted in `TemporaryPosts`, for the `NOT EXISTS`.
//! - **A direct channel exists**, with `TeamId = ''`, so the `TeamId = ? OR TeamId = ''` half of
//!   the team search has a row to lose.
//! - **Three files are re-dated straight in `FileInfo`** at the last millisecond of one day,
//!   noon of the next and the first millisecond of the one after, so `before:`, `on:` and
//!   `after:` each sit on a boundary and a `<` for a `<=` is a different answer.
//! - **One file's `Content` is set**, since nothing over REST extracts it, so the third
//!   `to_tsvector` arm is a filter and not a no-op.
//! - **A second team** holds one file the plain user cannot see, for the team scoping.
//! - **Uploads are spaced** so `ORDER BY CreateAt DESC` never ties, which is what keeps the byte
//!   comparison honest ([`memory: parity suite order-tie flakes`]).
//!
//! # Rows all begin `mmrsfilesearch`
//!
//! Not the shared `mmrs-parity-` prefix, for the reason `post_search` gives: `purge_api_fixtures`
//! runs once per binary and would delete another suite's team mid-run.
//!
//! # Every request holds the busy read guard
//!
//! Both routes are `DisableWhenBusy`, and `busy_gates` marks this server busy under the write
//! guard; a search made while it does would be a 503 that has nothing to do with search.

use std::time::Duration;

use crate::common;

use common::{
    BUSY_STATE, GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_direct_channel, fetch_both_raw, fixture_pool, go_minted_token, post_both_raw,
    post_message_with_files, stack_enabled, upload_file, username_of,
};

const PREFIX: &str = "mmrsfilesearch";
const PLAIN_PASSWORD: &str = "Mmrs-Plain-1234";

/// 2024-03-14T23:59:59.999Z — the last millisecond of the 14th, UTC.
const DATED_ONE_AT: i64 = 1_710_460_799_999;
/// 2024-03-15T12:00:00.000Z.
const DATED_TWO_AT: i64 = 1_710_504_000_000;
/// 2024-03-16T00:00:00.000Z — the first millisecond of the 16th, UTC.
const DATED_THREE_AT: i64 = 1_710_547_200_000;
/// +05:30, as the webapp sends it: seconds east of UTC.
const IST_OFFSET: i64 = 19_800;

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

/// Remove every row this suite authors, at the **start** of the run — an assertion panics past
/// trailing cleanup, so the next run's purge is the only one certain to happen.
async fn purge_file_search_fixtures() {
    let Some(pool) = fixture_pool().await else {
        return;
    };

    const TEAMS: &str = "SELECT id FROM teams WHERE name LIKE 'mmrsfilesearch%'";
    const USERS: &str = "SELECT id FROM users WHERE username LIKE 'mmrsfilesearch%'";

    let channel_ids: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT id FROM channels WHERE teamid IN ({TEAMS}) \
         OR (teamid = '' AND id IN (SELECT channelid FROM channelmembers WHERE userid IN ({USERS})))"
    ))
    .fetch_all(&pool)
    .await
    .unwrap_or_default();

    let posts = "SELECT id FROM posts WHERE channelid = ANY($1)";

    for statement in [
        "DELETE FROM fileinfo WHERE name LIKE 'mmrsfs%' OR channelid = ANY($1)".to_owned(),
        format!("DELETE FROM temporaryposts WHERE postid IN ({posts})"),
        format!("DELETE FROM threadmemberships WHERE postid IN ({posts})"),
        format!("DELETE FROM threads WHERE postid IN ({posts})"),
        "DELETE FROM posts WHERE channelid = ANY($1)".to_owned(),
        format!(
            "DELETE FROM sidebarchannels WHERE categoryid IN (SELECT id FROM sidebarcategories WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})) OR channelid = ANY($1)"
        ),
        format!("DELETE FROM sidebarcategories WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})"),
        "DELETE FROM channelmemberhistory WHERE channelid = ANY($1)".to_owned(),
        format!("DELETE FROM channelmembers WHERE channelid = ANY($1) OR userid IN ({USERS})"),
        format!("DELETE FROM publicchannels WHERE teamid IN ({TEAMS})"),
        "DELETE FROM channels WHERE id = ANY($1)".to_owned(),
        format!("DELETE FROM teammembers WHERE teamid IN ({TEAMS}) OR userid IN ({USERS})"),
        "DELETE FROM teams WHERE name LIKE 'mmrsfilesearch%'".to_owned(),
        format!("DELETE FROM preferences WHERE userid IN ({USERS})"),
        format!("DELETE FROM sessions WHERE userid IN ({USERS})"),
        "DELETE FROM users WHERE username LIKE 'mmrsfilesearch%'".to_owned(),
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
            "display_name": format!("mmrs file search {tag}"),
            "type": kind,
        }),
    )
    .await["id"]
        .as_str()
        .expect("a channel id")
        .to_owned()
}

/// Upload a text file to a channel, attach it to a post, and answer the file id; then a pause
/// so the next file's `CreateAt` is strictly later.
async fn post_file(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    name: &str,
    content_type: &str,
) -> (String, String) {
    let file_id = upload_file(client, token, channel_id, name, content_type, b"mmrs bytes").await;
    let post_id = post_message_with_files(
        client,
        token,
        channel_id,
        &format!("mmrsfs post for {name}"),
        std::slice::from_ref(&file_id),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(3)).await;
    (file_id, post_id)
}

async fn set_file_bigint(pool: &sqlx::PgPool, file_id: &str, column: &str, value: i64) {
    sqlx::query(&format!("UPDATE fileinfo SET {column} = $1 WHERE id = $2"))
        .bind(value)
        .bind(file_id)
        .execute(pool)
        .await
        .expect("the fileinfo column is written");
}

struct Fixture {
    team: String,
    /// A second team the plain user is not in, holding one `quebec` file.
    team2: String,
    admin_username: String,
    plain_username: String,
    plain_token: String,
    /// `main`, admin: `mmrsfs-alpha-report.pdf`.
    f_alpha: String,
    /// `main`, plain: `mmrsfs bravo notes.txt`.
    f_bravo: String,
    /// `main`, admin: `mmrsfs charlie.txt`, whose `Content` is `mmrsfs zulu inside`.
    f_charlie: String,
    /// `other`, admin: `mmrsfs delta.txt`.
    f_delta: String,
    /// `third` (public, plain not a member), admin: `mmrsfs echo.txt`.
    f_echo: String,
    /// `private`, admin: `mmrsfs foxtrot.txt`.
    f_foxtrot: String,
    /// `archived`, admin, posted before the archive: `mmrsfs golf.txt`.
    f_golf: String,
    /// `main`, admin: `mmrsfs hotel.txt`, soft-deleted in `FileInfo`.
    f_hotel: String,
    /// `main`, admin: `mmrsfs india.txt`, uploaded and never posted.
    f_india: String,
    /// `main`, admin: `mmrsfs juliet.txt`, whose post is in `TemporaryPosts`.
    f_juliet: String,
    /// The admin–plain direct channel, admin: `mmrsfs kilo.txt`.
    f_kilo: String,
    /// `main`, admin, re-dated to the three boundaries: `mmrsfs lima.txt`, `mmrsfs mike.txt`,
    /// `mmrsfs november.txt`.
    f_lima: String,
    f_mike: String,
    f_november: String,
    /// `team2`'s channel, admin: `mmrsfs quebec.txt`.
    f_quebec: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async { build_fixture(client, token).await })
        .await
}

async fn build_fixture(client: &reqwest::Client, admin_token: &str) -> Fixture {
    purge_file_search_fixtures().await;
    let pool = fixture_pool()
        .await
        .expect("DATABASE_URL is set for the parity stack");

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

    let team = go_post(
        client,
        admin_token,
        "/api/v4/teams",
        serde_json::json!({
            "name": format!("{PREFIX}-team"),
            "display_name": "mmrs file search",
            "type": "O",
        }),
    )
    .await["id"]
        .as_str()
        .expect("a team id")
        .to_owned();
    let team2 = go_post(
        client,
        admin_token,
        "/api/v4/teams",
        serde_json::json!({
            "name": format!("{PREFIX}-team2"),
            "display_name": "mmrs file search two",
            "type": "O",
        }),
    )
    .await["id"]
        .as_str()
        .expect("a team id")
        .to_owned();

    let plain_username = format!("{PREFIX}plain");
    let plain_id = create_member(client, admin_token, &team, &plain_username).await;
    let plain_token = login(client, &plain_username).await;

    let main = create_channel(client, admin_token, &team, "main", "O").await;
    let other = create_channel(client, admin_token, &team, "other", "O").await;
    let third = create_channel(client, admin_token, &team, "third", "O").await;
    let private = create_channel(client, admin_token, &team, "private", "P").await;
    let archived = create_channel(client, admin_token, &team, "archived", "O").await;
    let team2_channel = create_channel(client, admin_token, &team2, "far", "O").await;
    for channel in [&main, &other, &archived] {
        add_user_to_channel(client, admin_token, channel, &plain_id).await;
    }
    let direct = create_direct_channel(client, admin_token, &admin_id, &plain_id).await;

    let (f_alpha, _) = post_file(
        client,
        admin_token,
        &main,
        "mmrsfs-alpha-report.pdf",
        "application/pdf",
    )
    .await;
    let (f_bravo, _) = post_file(
        client,
        &plain_token,
        &main,
        "mmrsfs bravo notes.txt",
        "text/plain",
    )
    .await;
    let (f_charlie, _) = post_file(
        client,
        admin_token,
        &main,
        "mmrsfs charlie.txt",
        "text/plain",
    )
    .await;
    let (f_delta, _) = post_file(
        client,
        admin_token,
        &other,
        "mmrsfs delta.txt",
        "text/plain",
    )
    .await;
    let (f_echo, _) = post_file(client, admin_token, &third, "mmrsfs echo.txt", "text/plain").await;
    let (f_foxtrot, _) = post_file(
        client,
        admin_token,
        &private,
        "mmrsfs foxtrot.txt",
        "text/plain",
    )
    .await;
    let (f_golf, _) = post_file(
        client,
        admin_token,
        &archived,
        "mmrsfs golf.txt",
        "text/plain",
    )
    .await;
    let (f_hotel, _) =
        post_file(client, admin_token, &main, "mmrsfs hotel.txt", "text/plain").await;
    let f_india = upload_file(
        client,
        admin_token,
        &main,
        "mmrsfs india.txt",
        "text/plain",
        b"mmrs bytes",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(3)).await;
    let (f_juliet, juliet_post) = post_file(
        client,
        admin_token,
        &main,
        "mmrsfs juliet.txt",
        "text/plain",
    )
    .await;
    let (f_kilo, _) = post_file(
        client,
        admin_token,
        &direct,
        "mmrsfs kilo.txt",
        "text/plain",
    )
    .await;
    let (f_lima, _) = post_file(client, admin_token, &main, "mmrsfs lima.txt", "text/plain").await;
    let (f_mike, _) = post_file(client, admin_token, &main, "mmrsfs mike.txt", "text/plain").await;
    let (f_november, _) = post_file(
        client,
        admin_token,
        &main,
        "mmrsfs november.txt",
        "text/plain",
    )
    .await;
    let (f_quebec, _) = post_file(
        client,
        admin_token,
        &team2_channel,
        "mmrsfs quebec.txt",
        "text/plain",
    )
    .await;

    // The rows the REST API cannot shape.
    sqlx::query("UPDATE fileinfo SET content = 'mmrsfs zulu inside' WHERE id = $1")
        .bind(&f_charlie)
        .execute(&pool)
        .await
        .expect("content is written");
    set_file_bigint(&pool, &f_hotel, "deleteat", 1).await;
    sqlx::query(
        "INSERT INTO temporaryposts (postid, type, expireat) VALUES ($1, 'burn_on_read', 0)",
    )
    .bind(&juliet_post)
    .execute(&pool)
    .await
    .expect("the temporary post is planted");
    set_file_bigint(&pool, &f_lima, "createat", DATED_ONE_AT).await;
    set_file_bigint(&pool, &f_mike, "createat", DATED_TWO_AT).await;
    set_file_bigint(&pool, &f_november, "createat", DATED_THREE_AT).await;

    // Archive after the upload, so the file has a channel to be hidden with.
    let response = client
        .delete(format!("{GO}/api/v4/channels/{archived}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 200, "the channel archives");

    Fixture {
        team,
        team2,
        admin_username,
        plain_username,
        plain_token,
        f_alpha,
        f_bravo,
        f_charlie,
        f_delta,
        f_echo,
        f_foxtrot,
        f_golf,
        f_hotel,
        f_india,
        f_juliet,
        f_kilo,
        f_lima,
        f_mike,
        f_november,
        f_quebec,
    }
}

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

fn decode(body: &[u8], context: &str) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "{context}: body is not JSON ({e}): {}",
            String::from_utf8_lossy(body)
        )
    })
}

fn order_of(list: &serde_json::Value) -> Vec<String> {
    list["order"]
        .as_array()
        .expect("`order` is an array")
        .iter()
        .map(|v| v.as_str().expect("an id").to_owned())
        .collect()
}

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
    assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    decode(&go, context)
}

fn team_path(fixture: &Fixture) -> String {
    format!("/api/v4/teams/{}/files/search", fixture.team)
}

fn terms(terms: &str) -> serde_json::Value {
    serde_json::json!({ "terms": terms })
}

fn sorted(ids: &[&String]) -> Vec<String> {
    let mut ids: Vec<String> = ids.iter().map(|id| (*id).clone()).collect();
    ids.sort();
    ids
}

fn id_set(list: &serde_json::Value) -> Vec<String> {
    let mut ids = order_of(list);
    ids.sort();
    ids
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The plain user is in `main`, `other` and the archived channel: the membership join keeps
/// `third` (public, readable) and `private` out; `DeleteAt`, the orphan upload, the temporary
/// post and the archived channel each lose one more. What is left is ordered newest first.
#[tokio::test]
async fn a_plain_search_is_scoped_to_the_callers_channel_memberships() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin).await;

    let list = both_agree(
        &client,
        &fx.plain_token,
        &team_path(fx),
        terms("mmrsfs"),
        "plain, team",
    )
    .await;
    assert_eq!(
        id_set(&list),
        sorted(&[
            &fx.f_alpha,
            &fx.f_bravo,
            &fx.f_charlie,
            &fx.f_delta,
            &fx.f_kilo,
            &fx.f_lima,
            &fx.f_mike,
            &fx.f_november,
        ])
    );
    // Newest first — the three re-dated files sit at the end in date order.
    let order = order_of(&list);
    assert_eq!(
        &order[order.len() - 3..],
        &[fx.f_november.clone(), fx.f_mike.clone(), fx.f_lima.clone()]
    );
    assert_eq!(list["file_infos"][&fx.f_alpha]["archived"], false);
    assert_eq!(list["first_inaccessible_file_time"], 0);
    // Each of the four exclusions removed exactly its own row.
    let ids = id_set(&list);
    for (absent, why) in [
        (&fx.f_hotel, "DeleteAt = 0"),
        (&fx.f_india, "PostId <> ''"),
        (&fx.f_juliet, "NOT EXISTS TemporaryPosts"),
        (&fx.f_golf, "C.DeleteAt = 0"),
        (&fx.f_echo, "the membership join"),
    ] {
        assert!(!ids.contains(absent), "{why} should have removed the row");
    }
}

/// The admin is in nothing this suite made except through the posts (posting joins), so the
/// private and third channels appear for them; `include_deleted_channels` adds the archived one.
#[tokio::test]
async fn the_admin_sees_the_private_channel_and_include_deleted_channels_adds_the_archived_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin).await;

    let list = both_agree(
        &client,
        &admin,
        &team_path(fx),
        terms("mmrsfs"),
        "admin, team",
    )
    .await;
    assert_eq!(
        id_set(&list),
        sorted(&[
            &fx.f_alpha,
            &fx.f_bravo,
            &fx.f_charlie,
            &fx.f_delta,
            &fx.f_echo,
            &fx.f_foxtrot,
            &fx.f_kilo,
            &fx.f_lima,
            &fx.f_mike,
            &fx.f_november,
        ])
    );

    let list = both_agree(
        &client,
        &admin,
        &team_path(fx),
        serde_json::json!({ "terms": "mmrsfs", "include_deleted_channels": true }),
        "admin, team, include_deleted_channels",
    )
    .await;
    assert!(id_set(&list).contains(&fx.f_golf));
    assert_eq!(id_set(&list).len(), 11);
}

/// `is_or_search` widens the join; excluded terms negate; a search of only excluded terms
/// builds a query Postgres rejects and is an empty page on both servers; a prefix star matches.
#[tokio::test]
async fn or_search_excluded_terms_and_prefixes() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin).await;
    let path = team_path(fx);

    let list = both_agree(&client, &admin, &path, terms("mmrsfs alpha bravo"), "and").await;
    assert!(order_of(&list).is_empty(), "no file carries both words");

    let list = both_agree(
        &client,
        &admin,
        &path,
        serde_json::json!({ "terms": "alpha bravo", "is_or_search": true }),
        "or",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&fx.f_alpha, &fx.f_bravo]));

    let list = both_agree(
        &client,
        &admin,
        &path,
        terms("mmrsfs -alpha -bravo"),
        "excluded",
    )
    .await;
    assert!(!id_set(&list).contains(&fx.f_alpha));
    assert!(!id_set(&list).contains(&fx.f_bravo));
    assert!(id_set(&list).contains(&fx.f_charlie));

    let list = both_agree(&client, &admin, &path, terms("-alpha"), "only excluded").await;
    assert!(
        order_of(&list).is_empty(),
        "` & !(alpha)` is rejected by Postgres on both"
    );

    let list = both_agree(&client, &admin, &path, terms("char*"), "prefix").await;
    assert_eq!(id_set(&list), vec![fx.f_charlie.clone()]);

    let list = both_agree(&client, &admin, &path, terms("*"), "a lone star").await;
    assert!(
        order_of(&list).is_empty(),
        "`*` is dropped before the store"
    );
}

/// The hyphenated name matches through both `Name` arms; a term inside a hyphen matches because
/// hyphens become spaces (unlike the post search); `Content` is the third arm.
#[tokio::test]
async fn hyphens_and_content_are_searched() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin).await;
    let path = team_path(fx);

    let list = both_agree(
        &client,
        &admin,
        &path,
        terms("mmrsfs-alpha-report"),
        "hyphenated",
    )
    .await;
    assert_eq!(id_set(&list), vec![fx.f_alpha.clone()]);

    let list = both_agree(&client, &admin, &path, terms("report"), "inner word").await;
    assert_eq!(id_set(&list), vec![fx.f_alpha.clone()]);

    let list = both_agree(&client, &admin, &path, terms("zulu"), "content").await;
    assert_eq!(id_set(&list), vec![fx.f_charlie.clone()]);
}

/// `from:` and `-from:` by username; `in:` and `-in:` by channel name; `ext:` and `-ext:`.
#[tokio::test]
async fn from_in_and_ext_filters() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin).await;
    let path = team_path(fx);

    let list = both_agree(
        &client,
        &admin,
        &path,
        terms(&format!("mmrsfs from:{}", fx.plain_username)),
        "from plain",
    )
    .await;
    assert_eq!(id_set(&list), vec![fx.f_bravo.clone()]);

    let list = both_agree(
        &client,
        &admin,
        &path,
        terms(&format!("mmrsfs -from:{}", fx.admin_username)),
        "not from admin",
    )
    .await;
    assert_eq!(id_set(&list), vec![fx.f_bravo.clone()]);

    let list = both_agree(
        &client,
        &admin,
        &path,
        terms(&format!("mmrsfs in:{PREFIX}-other")),
        "in other",
    )
    .await;
    assert_eq!(id_set(&list), vec![fx.f_delta.clone()]);

    let list = both_agree(
        &client,
        &admin,
        &path,
        terms(&format!("mmrsfs -in:{PREFIX}-main")),
        "not in main",
    )
    .await;
    assert_eq!(
        id_set(&list),
        sorted(&[&fx.f_delta, &fx.f_echo, &fx.f_foxtrot, &fx.f_kilo])
    );

    let list = both_agree(&client, &admin, &path, terms("mmrsfs ext:pdf"), "ext pdf").await;
    assert_eq!(id_set(&list), vec![fx.f_alpha.clone()]);

    let list = both_agree(&client, &admin, &path, terms("mmrsfs -ext:txt"), "not txt").await;
    assert_eq!(id_set(&list), vec![fx.f_alpha.clone()]);

    // A flag with no terms is still a search.
    let list = both_agree(&client, &admin, &path, terms("ext:pdf"), "ext only").await;
    assert_eq!(id_set(&list), vec![fx.f_alpha.clone()]);

    // An unknown username stays a username and matches no id.
    let list = both_agree(
        &client,
        &admin,
        &path,
        terms("mmrsfs from:nobodyhere"),
        "from nobody",
    )
    .await;
    assert!(order_of(&list).is_empty());
}

/// The three re-dated files sit on day boundaries: `before:X` is `<=` the last ms of the day
/// **before** X, `after:X` is `>=` the first ms of the day **after** X, `on:` is the day;
/// `-on:` excludes it; and the time-zone offset moves the first file across the `on:` boundary.
#[tokio::test]
async fn date_modifiers_and_the_time_zone_offset() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin).await;
    let path = team_path(fx);

    let list = both_agree(
        &client,
        &admin,
        &path,
        terms("mmrsfs before:2024-03-15"),
        "before",
    )
    .await;
    assert_eq!(id_set(&list), vec![fx.f_lima.clone()]);

    let list = both_agree(
        &client,
        &admin,
        &path,
        terms("mmrsfs after:2024-03-15"),
        "after",
    )
    .await;
    let ids = id_set(&list);
    assert!(ids.contains(&fx.f_november) && !ids.contains(&fx.f_mike) && !ids.contains(&fx.f_lima));

    let list = both_agree(&client, &admin, &path, terms("mmrsfs on:2024-03-15"), "on").await;
    assert_eq!(id_set(&list), vec![fx.f_mike.clone()]);

    let list = both_agree(
        &client,
        &admin,
        &path,
        terms("mmrsfs -on:2024-03-15"),
        "not on",
    )
    .await;
    let ids = id_set(&list);
    assert!(!ids.contains(&fx.f_mike) && ids.contains(&fx.f_lima) && ids.contains(&fx.f_november));

    let list = both_agree(
        &client,
        &admin,
        &path,
        terms("mmrsfs -before:2024-03-15 -after:2024-03-15"),
        "excluded before and after",
    )
    .await;
    assert_eq!(id_set(&list), vec![fx.f_mike.clone()]);

    // +05:30: lima (23:59:59.999Z on the 14th) becomes 05:29 on the 15th.
    let list = both_agree(
        &client,
        &admin,
        &path,
        serde_json::json!({ "terms": "mmrsfs on:2024-03-15", "time_zone_offset": IST_OFFSET }),
        "on, IST",
    )
    .await;
    assert_eq!(id_set(&list), sorted(&[&fx.f_lima, &fx.f_mike]));
}

/// The all-teams route crosses both teams for the admin; the team route keeps `quebec` out;
/// the direct channel (`TeamId = ''`) is in both; and page 1 is empty.
#[tokio::test]
async fn the_all_teams_route_and_paging() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin).await;

    let list = both_agree(
        &client,
        &admin,
        "/api/v4/files/search",
        terms("mmrsfs"),
        "all teams",
    )
    .await;
    let ids = id_set(&list);
    assert!(ids.contains(&fx.f_quebec) && ids.contains(&fx.f_kilo) && ids.contains(&fx.f_alpha));

    let list = both_agree(&client, &admin, &team_path(fx), terms("mmrsfs"), "team").await;
    let ids = id_set(&list);
    assert!(!ids.contains(&fx.f_quebec) && ids.contains(&fx.f_kilo));

    let list = both_agree(
        &client,
        &admin,
        &team_path(fx),
        serde_json::json!({ "terms": "mmrsfs", "page": 1, "per_page": 2 }),
        "page 1",
    )
    .await;
    assert!(order_of(&list).is_empty());

    // `in:@user` resolves the direct channel.
    let list = both_agree(
        &client,
        &admin,
        &team_path(fx),
        terms(&format!("mmrsfs in:@{}", fx.plain_username)),
        "in:@plain",
    )
    .await;
    assert_eq!(id_set(&list), vec![fx.f_kilo.clone()]);
}

/// The plain user is not in `team2`: the team route is the `view_team` 403 before the body is
/// read; the all-teams route simply has nothing from that team.
#[tokio::test]
async fn a_team_the_caller_is_not_in_is_forbidden() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin).await;

    let err = both_refuse(
        &client,
        &fx.plain_token,
        &format!("/api/v4/teams/{}/files/search", fx.team2),
        b"not json",
        403,
        "plain, team2",
    )
    .await;
    assert_eq!(err["id"], "api.context.permissions.app_error");

    let list = both_agree(
        &client,
        &fx.plain_token,
        "/api/v4/files/search",
        terms("mmrsfs quebec"),
        "plain, all",
    )
    .await;
    assert!(order_of(&list).is_empty());
}

/// The body failures: not JSON, an empty body, missing and empty `terms`, a wrong type; and
/// the pinned `GET /files/search`, which is `get_file`'s 400 for a seven-character id.
#[tokio::test]
async fn the_body_failures_are_gos_400s() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let fx = fixture(&client, &admin).await;
    let path = team_path(fx);

    for (body, id, context) in [
        (
            &b"not json"[..],
            "api.post.search_files.invalid_body.app_error",
            "not json",
        ),
        (
            &b""[..],
            "api.post.search_files.invalid_body.app_error",
            "empty",
        ),
        (
            &br#"{"terms":5}"#[..],
            "api.post.search_files.invalid_body.app_error",
            "wrong type",
        ),
        (
            &br#"{}"#[..],
            "api.context.invalid_body_param.app_error",
            "no terms",
        ),
        (
            &br#"{"terms":""}"#[..],
            "api.context.invalid_body_param.app_error",
            "empty terms",
        ),
        (
            &b"null"[..],
            "api.context.invalid_body_param.app_error",
            "null",
        ),
    ] {
        let err = both_refuse(&client, &admin, &path, body, 400, context).await;
        assert_eq!(err["id"], id, "{context}");
    }

    let _not_busy = BUSY_STATE.read().await;
    let ((go_status, go), (rs_status, rs)) =
        fetch_both_raw(&client, &admin, "/api/v4/files/search").await;
    assert_eq!(go_status, 400, "{}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, 400, "{}", String::from_utf8_lossy(&rs));
    assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/files/search");
    let go = decode(&go, "GET /files/search");
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");

    // Unauthenticated: the session comes first on both.
    let response = client
        .post(format!("{RUST}/api/v4/files/search"))
        .body("{}")
        .send()
        .await
        .expect("Rust answers");
    assert_eq!(response.status(), 401);
}
