//! Cross-server parity for `GET /api/v4/users/{user_id}/channel_members` — every channel the
//! caller belongs to, across every team. The webapp asks for it once per load.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity channel_members_for_user
//! ```
//!
//! # Two responses behind one path
//!
//! `?page=-1` selects a newline-delimited stream (`application/x-ndjson`); anything else —
//! including no `page` at all — selects a JSON array. [`the_page_sentinel_chooses_the_encoding`].
//!
//! # And the stream stops on a 404 it only sometimes swallows
//!
//! The cursor store call raises `ErrNotFound` for an empty page, and the loop reads that as
//! "done" — but only once it holds a cursor. A caller whose *first* page is empty gets the 404
//! itself. [`a_user_with_no_memberships_gets_the_404_the_stream_would_have_swallowed`].

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel_typed, create_plain_user, create_team, fetch_both_raw, fetch_both_stable,
    go_minted_token, logged_in_user_id, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    plain_id: String,
    plain_token: String,
    /// A second plain user whose channel memberships are deleted straight from the table, so the
    /// streaming branch's first page is empty.
    stripped_id: String,
    stripped_token: String,
    /// A channel in a team, so the three team columns are non-blank for at least one row.
    team_channel_id: String,
    /// The direct message the plain user is in. It has **no team**, which is the only way to
    /// reach the three `COALESCE`s in the select list.
    dm_channel_id: String,
    /// A third user carrying more than one page worth of memberships, planted directly. Without
    /// it the streaming loop runs exactly once and its cursor, its page-size test and its
    /// advance are all dead code to the suite — three mutations survived on that.
    paged_id: String,
    paged_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;

            let team_id = create_team(client, token, "cmfuteam").await;
            let plain = create_plain_user(client, token, &team_id, "cmfu").await;

            // Three channels, so a `per_page` smaller than the total splits the list.
            let mut team_channel_id = String::new();
            for (tag, kind) in [("cmfua", "O"), ("cmfub", "P"), ("cmfuc", "O")] {
                let id = create_channel_typed(client, token, &team_id, tag, kind).await;
                add_user_to_channel(client, token, &id, &plain.id).await;
                if tag == "cmfua" {
                    team_channel_id = id;
                }
            }

            // A user with no memberships at all. Go joins every new team member to the default
            // channels, so the rows have to be deleted directly — the REST API cannot leave a
            // user in this state, and it is the only shape that reaches the stream's own 404.
            let stripped = create_plain_user(client, token, &team_id, "cmfustrip").await;
            strip_channel_memberships(&stripped.id).await;

            // A direct message gives the plain user one row whose channel has no team.
            let dm_channel_id =
                open_direct_channel(client, &plain.token, &plain.id, logged_in_user_id()).await;

            // 150 memberships, so the hundred-at-a-time walk takes two turns and its first page
            // comes back exactly full. Planted with one statement: creating that many channels
            // over REST would dominate the suite's runtime.
            let paged = create_plain_user(client, token, &team_id, "cmfupaged").await;
            plant_many_memberships(&paged.id, 150).await;

            Fixture {
                plain_id: plain.id,
                plain_token: plain.token,
                stripped_id: stripped.id,
                stripped_token: stripped.token,
                team_channel_id,
                dm_channel_id,
                paged_id: paged.id,
                paged_token: paged.token,
            }
        })
        .await
}

/// Give `user_id` membership of `count` existing channels, straight into the table.
///
/// The REST API would need `count` channel creations and joins to reach the same state, which is
/// minutes of fixture time for a property — that the streaming walk pages correctly — that needs
/// nothing else about those channels to be true.
async fn plant_many_memberships(user_id: &str, count: i64) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return;
    };
    sqlx::query(
        "INSERT INTO channelmembers (channelid, userid, roles, lastviewedat, msgcount, \
             mentioncount, notifyprops, lastupdateat, schemeuser, schemeadmin, schemeguest, \
             mentioncountroot, msgcountroot, urgentmentioncount) \
         SELECT c.id, $1, 'channel_user', 0, 0, 0, '{}', 0, true, false, false, 0, 0, 0 \
           FROM channels c \
          WHERE c.type <> 'S' \
          ORDER BY c.id \
          LIMIT $2 \
         ON CONFLICT (channelid, userid) DO NOTHING",
    )
    .bind(user_id)
    .bind(count)
    .execute(&pool)
    .await
    .expect("the insert runs");
}

async fn open_direct_channel(client: &reqwest::Client, token: &str, a: &str, b: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([a, b]))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "opening the DM failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Remove every `ChannelMembers` row for a user. Silent without a `DATABASE_URL`; the test that
/// needs it re-checks and says so.
async fn strip_channel_memberships(user_id: &str) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return;
    };
    sqlx::query("DELETE FROM channelmembers WHERE userid = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("the delete runs");
}

fn path(user_id: &str, query: &str) -> String {
    if query.is_empty() {
        format!("/api/v4/users/{user_id}/channel_members")
    } else {
        format!("/api/v4/users/{user_id}/channel_members?{query}")
    }
}

fn channel_ids(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .expect("decodes")
        .as_array()
        .expect("an array")
        .iter()
        .map(|m| m["channel_id"].as_str().expect("an id").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

/// The paginated branch, byte for byte — which also pins the column set, the team columns, and
/// the trailing newline.
#[tokio::test]
async fn a_page_of_memberships_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, "page=0&per_page=200");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{p} must be byte-identical"
    );
    assert_eq!(
        go.last(),
        Some(&b'\n'),
        "json.NewEncoder(w).Encode appends the newline json.Marshal does not"
    );

    let ids = channel_ids(&go);
    assert!(
        ids.contains(&f.team_channel_id),
        "the fixture channel is in the list: {ids:?}"
    );

    // `ORDER BY ChannelId ASC` is what the cursor branch walks; assert it here where the whole
    // page is visible.
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "the list is ordered by channel id");

    // The three team columns are present on every row, and non-blank for a channel in a team.
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let team_row = parsed
        .as_array()
        .expect("an array")
        .iter()
        .find(|m| m["channel_id"] == f.team_channel_id.as_str())
        .expect("the fixture channel is in the page");
    assert!(
        !team_row["team_name"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "a channel in a team carries its team's name: {team_row}"
    );
    assert!(team_row["team_update_at"].as_i64().unwrap_or(0) > 0);
}

/// `?page=-1` is a sentinel, not a page number, and it changes the content type.
#[tokio::test]
async fn the_page_sentinel_chooses_the_encoding() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for (query, content_type, first_byte) in [
        ("", "application/json", b'['),
        ("page=0", "application/json", b'['),
        ("page=-1", "application/x-ndjson", b'{'),
    ] {
        let p = path(&f.plain_id, query);
        let mut bodies = Vec::new();
        for base in [GO, RUST] {
            let response = client
                .get(format!("{base}{p}"))
                .header("Authorization", format!("Bearer {}", f.plain_token))
                .send()
                .await
                .expect("the server answers");
            assert_eq!(response.status(), 200, "{base}{p}");
            assert_eq!(
                response
                    .headers()
                    .get("Content-Type")
                    .and_then(|v| v.to_str().ok()),
                Some(content_type),
                "{base}{p}"
            );
            bodies.push(response.bytes().await.expect("body").to_vec());
        }
        assert_eq!(
            String::from_utf8_lossy(&bodies[0]),
            String::from_utf8_lossy(&bodies[1]),
            "{p} must be byte-identical"
        );
        assert_eq!(bodies[0].first(), Some(&first_byte), "{p}: the shape");
    }
}

/// The stream is one object per line, and the same rows the array branch returns.
#[tokio::test]
async fn the_stream_carries_the_same_rows_one_per_line() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let stream = path(&f.plain_id, "page=-1");
    let (go_stream, rs_stream) = fetch_both_stable(&client, &f.plain_token, &stream).await;
    assert_eq!(go_stream, rs_stream, "{stream} must be byte-identical");

    let lines: Vec<&[u8]> = go_stream
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .collect();
    let streamed: Vec<String> = lines
        .iter()
        .map(|line| {
            serde_json::from_slice::<serde_json::Value>(line).expect("each line is one object")
                ["channel_id"]
                .as_str()
                .expect("an id")
                .to_owned()
        })
        .collect();

    let array = path(&f.plain_id, "page=0&per_page=200");
    let (go_array, _rs) = fetch_both_stable(&client, &f.plain_token, &array).await;
    assert_eq!(
        streamed,
        channel_ids(&go_array),
        "the two encodings carry the same rows in the same order"
    );
}

/// The stream's terminator is a 404 — and it is only swallowed once the walk has a cursor.
#[tokio::test]
async fn a_user_with_no_memberships_gets_the_404_the_stream_would_have_swallowed() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The stripping has to have happened, or this is a test about an ordinary user.
    if let Ok(url) = std::env::var("DATABASE_URL")
        && let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(&url)
            .await
    {
        let rows: (i64,) = sqlx::query_as("SELECT count(*) FROM channelmembers WHERE userid = $1")
            .bind(&f.stripped_id)
            .fetch_one(&pool)
            .await
            .expect("the count runs");
        assert_eq!(rows.0, 0, "the fixture user must have no memberships left");
    }

    let p = path(&f.stripped_id, "page=-1");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.stripped_token, &p).await;
    assert_eq!(
        go_status, 404,
        "with no cursor yet, the empty page is the answer rather than the terminator"
    );
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "app.channel.get_member.missing.app_error");

    // The array branch has no such guard: the same user gets an empty list.
    let array = path(&f.stripped_id, "page=0");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.stripped_token, &array).await;
    assert_eq!(go_status, 200, "the paginated branch answers `[]`");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go_body).trim_end(),
        "[]",
        "empty list, not null"
    );
    assert_eq!(go_body, rs_body, "{array} must be byte-identical");
}

/// `SanitizeForCurrentUser` blanks another user's counters to `-1` and leaves the caller's own.
#[tokio::test]
async fn another_users_rows_are_sanitised_for_the_admin() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, "page=0&per_page=200");
    let (go, rs) = fetch_both_stable(&client, &token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    for member in parsed.as_array().expect("an array") {
        assert_eq!(
            member["last_viewed_at"], -1,
            "the admin is not this member, so the counters are blanked: {member}"
        );
        assert_eq!(member["last_update_at"], -1);
    }

    // The same rows read by their owner keep their values, so the assertion above is about the
    // sanitiser and not about a fixture whose counters happen to be -1.
    let (go_own, _rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    let own: serde_json::Value = serde_json::from_slice(&go_own).expect("JSON");
    assert!(
        own.as_array()
            .expect("an array")
            .iter()
            .all(|m| m["last_update_at"].as_i64().unwrap_or(-1) > 0),
        "the owner sees real timestamps"
    );
}

/// The gate is `SessionHasPermissionToUser`, and its refusal names a write permission.
#[tokio::test]
async fn another_users_memberships_are_a_403_for_a_plain_caller() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let admin_id = logged_in_user_id();

    let p = path(admin_id, "page=0");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.plain_token, &p).await;
    assert_eq!(go_status, 403, "a plain user cannot read the admin's list");
    assert_eq!(rs_status, go_status, "{p}: statuses must match");
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
    assert_eq!(go["id"], "api.context.permissions.app_error");
}

/// Pagination: a page smaller than the list splits it, and a page past the end is empty.
#[tokio::test]
async fn the_page_and_per_page_parameters_walk_the_list() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let all = path(&f.plain_id, "page=0&per_page=200");
    let (go_all, _rs) = fetch_both_stable(&client, &f.plain_token, &all).await;
    let every = channel_ids(&go_all);
    assert!(every.len() >= 3, "the fixture needs a few rows: {every:?}");

    let first = path(&f.plain_id, "page=0&per_page=2");
    let (go_first, rs_first) = fetch_both_stable(&client, &f.plain_token, &first).await;
    assert_eq!(go_first, rs_first, "{first} must be byte-identical");
    assert_eq!(
        channel_ids(&go_first),
        every[..2],
        "the first two, in order"
    );

    let second = path(&f.plain_id, "page=1&per_page=2");
    let (go_second, rs_second) = fetch_both_stable(&client, &f.plain_token, &second).await;
    assert_eq!(go_second, rs_second, "{second} must be byte-identical");
    assert_eq!(
        channel_ids(&go_second),
        every[2..4.min(every.len())],
        "page one is the next two, so `page` really is multiplied here"
    );

    let past = path(&f.plain_id, "page=500&per_page=2");
    let (go_past, rs_past) = fetch_both_stable(&client, &f.plain_token, &past).await;
    assert_eq!(go_past, rs_past, "{past} must be byte-identical");
    assert_eq!(String::from_utf8_lossy(&go_past).trim_end(), "[]");
}

/// `RequireUserId`: alphanumeric, so the router's charset lets it through, but the wrong length.
#[tokio::test]
async fn a_user_id_of_the_wrong_length_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for id in ["short", "aaaaaaaaaaaaaaaaaaaaaaaaaaa"] {
        let p = path(id, "page=0");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &p).await;
        assert_eq!(go_status, 400, "{p}");
        assert_eq!(rs_status, go_status, "{p}: statuses must match");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &p);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// Everything but `GET` on this path stays Go's.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, "");
    for method in [reqwest::Method::POST, reqwest::Method::DELETE] {
        let rs = client
            .request(method.clone(), format!("{RUST}{p}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            rs.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {p} must be forwarded"
        );
    }
}

/// An unauthenticated request never reaches the handler.
#[tokio::test]
async fn no_session_is_a_401_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let p = "/api/v4/users/aaaaaaaaaaaaaaaaaaaaaaaaaa/channel_members";

    let go = client
        .get(format!("{GO}{p}"))
        .send()
        .await
        .expect("Go answers");
    let rs = client
        .get(format!("{RUST}{p}"))
        .send()
        .await
        .expect("we answer");
    assert_eq!(go.status(), 401);
    assert_eq!(rs.status(), go.status(), "{p}: statuses must match");
    let go_body = go.bytes().await.expect("body").to_vec();
    let rs_body = rs.bytes().await.expect("body").to_vec();
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, p);
}

/// The three `COALESCE`s in the select list, reached by the one row whose channel has no team.
///
/// Every other fixture channel belongs to a team, so the fallbacks were dead code and a mutation
/// changing one of them survived. A direct message is the shape that exercises them.
#[tokio::test]
async fn a_channel_with_no_team_carries_blank_team_columns() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let p = path(&f.plain_id, "page=0&per_page=200");
    let (go, rs) = fetch_both_stable(&client, &f.plain_token, &p).await;
    assert_eq!(go, rs, "{p} must be byte-identical");

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let dm = parsed
        .as_array()
        .expect("an array")
        .iter()
        .find(|m| m["channel_id"] == f.dm_channel_id.as_str())
        .expect("the direct message is in the page");
    assert_eq!(dm["team_name"], "", "a DM belongs to no team");
    assert_eq!(dm["team_display_name"], "");
    assert_eq!(dm["team_update_at"], 0);

    // And a channel that *does* have a team is not blank, so the assertion above is about the
    // fallback rather than about the join being broken for everyone.
    let team_row = parsed
        .as_array()
        .expect("an array")
        .iter()
        .find(|m| m["channel_id"] == f.team_channel_id.as_str())
        .expect("the team channel is in the page");
    assert!(
        !team_row["team_name"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
    );
}

/// The streaming walk over **more than one page** — the only way its cursor, its page-size test
/// and its advance are exercised at all.
///
/// The fixture user holds 150 memberships, so the first page comes back exactly full (100) and
/// the second finishes the walk. Three mutations survived before this test existed: `>` widened
/// to `>=` on the cursor (which repeats a row), `<` widened to `<=` on the page-size check
/// (which stops after the first page), and `.last()` swapped for `.first()` (which walks the
/// same page forever, or backwards).
#[tokio::test]
async fn the_stream_walks_past_the_first_page_without_repeating_or_dropping_a_row() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let stream = path(&f.paged_id, "page=-1");
    let (go, rs) = fetch_both_stable(&client, &f.paged_token, &stream).await;
    assert_eq!(go, rs, "{stream} must be byte-identical");

    let ids: Vec<String> = go
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|line| {
            serde_json::from_slice::<serde_json::Value>(line).expect("one object per line")
                ["channel_id"]
                .as_str()
                .expect("an id")
                .to_owned()
        })
        .collect();

    assert!(
        ids.len() > 100,
        "the fixture must span more than one page, or the loop runs once: {}",
        ids.len()
    );

    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "no channel is streamed twice");

    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(
        ids, sorted,
        "the walk stays in channel-id order across the page boundary"
    );

    // And the array branch agrees on the whole set, so nothing was dropped at the seam.
    let array = path(&f.paged_id, "page=0&per_page=200");
    let (go_array, _rs) = fetch_both_stable(&client, &f.paged_token, &array).await;
    assert_eq!(
        ids,
        channel_ids(&go_array),
        "the two encodings carry the same rows across a page boundary"
    );
}

/// Sanitisation on the **streaming** branch. The stream test above reads the caller's own list,
/// where the sanitiser is a no-op; this one has the admin read somebody else's.
#[tokio::test]
async fn the_stream_sanitises_another_users_rows_too() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let stream = path(&f.plain_id, "page=-1");
    let (go, rs) = fetch_both_stable(&client, &token, &stream).await;
    assert_eq!(go, rs, "{stream} must be byte-identical");

    let lines: Vec<serde_json::Value> = go
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|line| serde_json::from_slice(line).expect("one object per line"))
        .collect();
    assert!(!lines.is_empty(), "the fixture user has memberships");
    for member in &lines {
        assert_eq!(
            member["last_viewed_at"], -1,
            "the admin is not this member, so the stream blanks the counters too: {member}"
        );
        assert_eq!(member["last_update_at"], -1);
    }
}
