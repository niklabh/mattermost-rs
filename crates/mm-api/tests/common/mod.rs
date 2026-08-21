//! Shared plumbing for the cross-server parity tests.
//!
//! Compiled separately into *each* integration-test binary, which has two consequences: the
//! `OnceCell` below is per-binary — one login per test file, not one per test — and anything a
//! given test file does not use looks unused from that binary's point of view, hence the
//! `dead_code` allowance.
#![allow(dead_code)]

use std::time::Duration;

pub const GO: &str = "http://localhost:8065";
pub const RUST: &str = "http://127.0.0.1:8066";
pub const LOGIN_ID: &str = "slice@example.com";
pub const PASSWORD: &str = "Slice-Test-1234";

/// True when the caller asked for the stack-backed tests.
///
/// Without this, every parity test returns early. Deliberate: `cargo test` on a machine with no
/// Docker must stay green, and a test that silently passes because it could not reach anything is
/// worse than one that is explicitly skipped.
pub fn stack_enabled() -> bool {
    std::env::var("MM_PARITY_STACK").is_ok_and(|v| v == "1")
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("client builds")
}

/// One login per test binary.
///
/// This is not an optimisation. **A login mutates the user row** — it bumps `UpdateAt`, which
/// appears in `/users/me`'s body and in its etag. With a login per test, tests running in
/// parallel move `UpdateAt` underneath each other and a byte comparison fails against a
/// seconds-old value while both servers were in fact perfectly agreed. One login removes the only
/// writer, so a diff means a real divergence.
static TOKEN: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

/// Log in against the **Go** server and return the token it mints.
///
/// The token has to come from Go: the whole point is that a credential this port never issued is
/// nonetheless accepted by it, against a row it never wrote.
pub async fn go_minted_token(client: &reqwest::Client) -> String {
    TOKEN
        .get_or_init(|| async {
            let response = client
                .post(format!("{GO}/api/v4/users/login"))
                .json(&serde_json::json!({ "login_id": LOGIN_ID, "password": PASSWORD }))
                .send()
                .await
                .expect("the Go server is reachable — is `docker compose up -d` running?");

            assert_eq!(
                response.status(),
                200,
                "login against Go failed; the fixture user may not exist yet"
            );

            let token = response
                .headers()
                .get("token")
                .expect("Go returns the session token in a `Token` header")
                .to_str()
                .expect("the token is ASCII")
                .to_owned();

            // The login body is the user, so the id comes from the same round trip. Tests used to
            // hardcode it, which broke the moment [D-130] required recreating the volume: ids are
            // minted per database, and a stale one fails as "permission denied" rather than as
            // "that user does not exist".
            let user: serde_json::Value = response.json().await.expect("login returns the user");
            let id = user["id"]
                .as_str()
                .expect("the user carries an id")
                .to_owned();
            USER_ID.set(id).expect("set once");

            token
        })
        .await
        .clone()
}

static USER_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The logged-in user's id. Panics unless [`go_minted_token`] has run, which every caller does
/// first because it is how they get a token at all.
pub fn logged_in_user_id() -> &'static str {
    USER_ID
        .get()
        .expect("call go_minted_token first — the id comes from the login response")
}

/// Every response from the Rust server carries `x-mmrs-served-by`: `rust` when we served it,
/// `go` when the proxy forwarded it. A parity test that compares a *forwarded* response is
/// comparing Go against Go and passes no matter what the handler does.
///
/// **This is not hypothetical.** The channel-member route's parity suite passed on its first run
/// while every request was being forwarded, because a stale `mm-api` process from an earlier
/// session still held port 8066 and the freshly built binary had silently failed to bind. Five
/// green tests, zero of them touching the code under test. The header was already there; nothing
/// was checking it.
fn assert_served_by_rust(headers: &reqwest::header::HeaderMap, path: &str) {
    let served_by = headers
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok());
    assert_eq!(
        served_by,
        Some("rust"),
        "{path} was forwarded to Go, so this comparison proves nothing about the Rust handler. \
         Is an older `mm-api` still bound to 8066, or the route not registered?"
    );
}

/// Fetch a path from both servers with the same token, returning `(go_body, rust_body)`.
///
/// Asserts the Rust side actually served it — see [`assert_served_by_rust`].
pub async fn fetch_both(client: &reqwest::Client, token: &str, path: &str) -> (Vec<u8>, Vec<u8>) {
    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        assert_eq!(response.status(), 200, "{base}{path} should return 200");
        if base == RUST {
            assert_served_by_rust(response.headers(), path);
        }
        response.bytes().await.expect("body reads").to_vec()
    };

    (get(GO).await, get(RUST).await)
}

/// Like [`fetch_both`] but without the 200 assertion: returns `(status, body)` from each server.
///
/// Error responses are wire format too — the webapp branches on `id` — so a route is only really
/// verified when its refusals match as well as its successes.
pub async fn fetch_both_raw(
    client: &reqwest::Client,
    token: &str,
    path: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        if base == RUST {
            assert_served_by_rust(response.headers(), path);
        }
        (status, response.bytes().await.expect("body reads").to_vec())
    };

    (get(GO).await, get(RUST).await)
}

/// The first channel of the first team the fixture user belongs to.
pub async fn a_channel_the_user_is_in(client: &reqwest::Client, token: &str) -> String {
    a_team_and_channel_the_user_is_in(client, token).await.1
}

/// `(team_id, channel_id)`, discovered through Go's own API. Ids are minted per database, so
/// hardcoding one survives only until the volume is recreated — which [D-130] required.
pub async fn a_team_and_channel_the_user_is_in(
    client: &reqwest::Client,
    token: &str,
) -> (String, String) {
    let teams: serde_json::Value = client
        .get(format!("{GO}/api/v4/users/me/teams"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("teams decode");
    let team_id = teams
        .as_array()
        .and_then(|t| t.first())
        .and_then(|t| t["id"].as_str())
        .expect("the fixture user belongs to at least one team");

    let channels: serde_json::Value = client
        .get(format!("{GO}/api/v4/users/me/teams/{team_id}/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("channels decode");

    let channel_id = channels
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["id"].as_str())
        .expect("the fixture user is in at least one channel")
        .to_owned();

    (team_id.to_owned(), channel_id)
}

/// Compare two error bodies and assert they differ in **exactly** the two keys that are known to,
/// returning the parsed Go body for further assertions.
///
/// `request_id` is per-request and can never match. `message` is Go's *translated* prose where
/// ours is the raw error id — the one remaining third of [D-092], which needs the i18n bundle.
///
/// Written as a difference-set assertion rather than as "compare these three fields" on purpose:
/// a field added to `AppError` upstream, or a value we get wrong in some *other* key, fails this
/// immediately instead of slipping through a hand-listed comparison. When i18n lands, `message`
/// comes out of the tolerated set and this gets stricter with a one-word edit.
pub fn assert_error_bodies_match_except_known_gaps(
    go_body: &[u8],
    rs_body: &[u8],
    context: &str,
) -> serde_json::Value {
    let go: serde_json::Value = serde_json::from_slice(go_body)
        .unwrap_or_else(|e| panic!("{context}: Go's body is not JSON: {e}"));
    let rs: serde_json::Value = serde_json::from_slice(rs_body)
        .unwrap_or_else(|e| panic!("{context}: our body is not JSON: {e}"));

    let go_obj = go.as_object().expect("an object");
    let rs_obj = rs.as_object().expect("an object");

    assert_eq!(
        go_obj.keys().collect::<Vec<_>>(),
        rs_obj.keys().collect::<Vec<_>>(),
        "{context}: the two bodies must carry the same keys in the same order"
    );

    let differing: Vec<&str> = go_obj
        .iter()
        .filter(|(key, value)| rs_obj.get(*key) != Some(*value))
        .map(|(key, _)| key.as_str())
        .collect();

    assert_eq!(
        differing,
        vec!["message", "request_id"],
        "{context}: only `message` (D-092, i18n) and `request_id` may differ.\n  go:   {go}\n  rust: {rs}"
    );

    // And pin what our `message` actually is, so the divergence stays the documented one rather
    // than becoming some third value nobody chose.
    assert_eq!(
        rs_obj.get("message"),
        rs_obj.get("id"),
        "{context}: until i18n lands our message is the raw id (D-092)"
    );

    go
}

/// Every channel of the fixture user's first team, in Go's order.
pub async fn channels_of_the_users_team(
    client: &reqwest::Client,
    token: &str,
) -> (String, Vec<String>) {
    let (team_id, _) = a_team_and_channel_the_user_is_in(client, token).await;
    let channels: serde_json::Value = client
        .get(format!("{GO}/api/v4/users/me/teams/{team_id}/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("channels decode");

    let ids = channels
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|c| c["id"].as_str().map(str::to_owned))
        .collect();
    (team_id, ids)
}

/// A freshly created **non-admin** user with its own token, created through Go's API.
///
/// The fixture user is a `system_admin`, and `manage_system` grants at branch 5 of
/// `SessionHasPermissionToChannel` — so every permission question answers "yes" for it, and a
/// parity test using it cannot tell which permission a handler asks for. Anything asserting that
/// a *specific* permission gates a route needs an actor who can be refused.
pub struct PlainUser {
    pub id: String,
    pub token: String,
}

/// Create a non-admin user, put it in `team_id`, and log it in. Caller cleans up with
/// [`delete_plain_user`].
pub async fn create_plain_user(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    tag: &str,
) -> PlainUser {
    let username = format!("mmrsplain{tag}");
    let password = "Mmrs-Plain-1234";

    let response = client
        .post(format!("{GO}/api/v4/users"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "email": format!("{username}@mmrs.invalid"),
            "username": username,
            "password": password,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the plain user failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the user decodes");
    let id = created["id"].as_str().expect("an id").to_owned();

    let joined = client
        .post(format!("{GO}/api/v4/teams/{team_id}/members"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "team_id": team_id, "user_id": id }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        joined.status().is_success(),
        "adding the plain user to the team failed: {}",
        joined.text().await.unwrap_or_default()
    );

    let login = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username, "password": password }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(login.status(), 200, "the plain user cannot log in");
    let token = login
        .headers()
        .get("token")
        .expect("Go returns a token header")
        .to_str()
        .expect("ASCII")
        .to_owned();

    PlainUser { id, token }
}

/// Best-effort teardown; deliberately ignores failures so a panicking test still tries.
pub async fn delete_plain_user(client: &reqwest::Client, admin_token: &str, user_id: &str) {
    let _ = client
        .delete(format!("{GO}/api/v4/users/{user_id}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await;
}

/// Create a channel on `team_id` through Go's API and return its id.
///
/// Tests that need a *particular* fixture shape make their own rather than reaching for whatever
/// the development database happens to hold. Two reasons, one of each kind: a shared channel's
/// membership can change under a test (and did — see the notes for 2026-08-20), and a test that
/// silently depends on "the team has at least two channels" fails for a reason that has nothing
/// to do with the code under test.
pub async fn create_channel(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    tag: &str,
) -> String {
    let name = format!("mmrs-parity-{tag}");
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": name,
            "display_name": format!("mmrs parity {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Best-effort teardown for [`create_channel`].
pub async fn delete_channel(client: &reqwest::Client, admin_token: &str, channel_id: &str) {
    let _ = client
        .delete(format!("{GO}/api/v4/channels/{channel_id}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await;
}

/// Add `user_id` to `channel_id` through Go's API.
pub async fn add_user_to_channel(
    client: &reqwest::Client,
    admin_token: &str,
    channel_id: &str,
    user_id: &str,
) {
    let response = client
        .post(format!("{GO}/api/v4/channels/{channel_id}/members"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "user_id": user_id }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "adding {user_id} to {channel_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Post `message` to `channel_id` as whoever holds `token`, returning the new post's id.
///
/// Unread counters are the *difference* between a channel's total and a member's own count, so a
/// test that wants a non-zero one needs somebody else to have said something. Posting as the
/// reader would move both numbers together and prove nothing.
///
/// `root_id` is what separates the `_root` counters from their siblings: a reply raises
/// `TotalMsgCount` and leaves `TotalMsgCountRoot` alone, so a fixture with no thread in it makes
/// `msg_count` and `msg_count_root` equal — and two columns holding equal values cannot catch a
/// port that reads the wrong one.
pub async fn post_message(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    message: &str,
    root_id: Option<&str>,
) -> String {
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
        "posting to {channel_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the post decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Mark `channel_id` read for whoever holds `token` — `POST /channels/members/me/view`.
///
/// This is what makes `ChannelMembers.MsgCount` non-zero, and therefore what makes
/// `TotalMsgCount - MsgCount` a *subtraction* rather than a copy. A fixture whose reader has
/// never viewed the channel leaves the member's count at `0`, and dropping the subtraction
/// entirely then produces the same answer — a mutation that survived until this existed.
pub async fn view_channel(client: &reqwest::Client, token: &str, channel_id: &str) {
    let response = client
        .post(format!("{GO}/api/v4/channels/members/me/view"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "channel_id": channel_id }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "viewing {channel_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Set one `ChannelMembers` column to SQL NULL, straight through the shared database.
///
/// Reserved for the columns Go's queries `COALESCE`: nothing in the REST API can produce a NULL
/// `UrgentMentionCount`, so the only way to exercise the coalesce — and to catch its removal — is
/// to write the NULL directly. Returns `false` when `DATABASE_URL` is unset, so the caller can
/// skip rather than fail.
pub async fn null_out_member_column(channel_id: &str, user_id: &str, column: &str) -> bool {
    assert_eq!(
        column, "urgentmentioncount",
        "only the coalesced column is allowed here; widening this needs a reason"
    );
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
    else {
        return false;
    };
    sqlx::query(
        "UPDATE channelmembers SET urgentmentioncount = NULL WHERE channelid = $1 AND userid = $2",
    )
    .bind(channel_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("the fixture member's column is nulled");
    true
}

/// Set one channel-member notify prop through Go's API, leaving the others alone.
pub async fn set_member_notify_prop(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    user_id: &str,
    key: &str,
    value: &str,
) {
    let response = client
        .put(format!(
            "{GO}/api/v4/channels/{channel_id}/members/{user_id}/notify_props"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ key: value }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "setting {key}={value} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// The username Go minted for `user_id`, needed to build an `@mention` that actually mentions.
pub async fn username_of(client: &reqwest::Client, admin_token: &str, user_id: &str) -> String {
    let user: serde_json::Value = client
        .get(format!("{GO}/api/v4/users/{user_id}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the user decodes");
    user["username"].as_str().expect("a username").to_owned()
}

/// Compare a path across both servers on a row that may still be **changing**.
///
/// Joining or creating a channel kicks off async work in Go — a system post, then unread-count
/// updates — so a membership row written moments ago is not quiescent. A plain `go` then `rust`
/// comparison caught exactly that: Go answered `mention_count: 1, last_update_at: …270` and we
/// answered `mention_count: 0, …265`, milliseconds apart, both correct for the instant each read.
///
/// So this reads Go, then Rust, then **Go again**, and only compares when Go's two reads agree —
/// evidence the row did not move across the window the Rust read sat in. It retries with a
/// growing pause and, if the row never settles, says *that* rather than reporting a divergence:
/// "the fixture never stopped changing" and "the two servers disagree" are different findings and
/// must not wear the same failure message.
pub async fn fetch_both_stable(
    client: &reqwest::Client,
    token: &str,
    path: &str,
) -> (Vec<u8>, Vec<u8>) {
    const ATTEMPTS: u64 = 8;

    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        assert_eq!(response.status(), 200, "{base}{path} should return 200");
        if base == RUST {
            assert_served_by_rust(response.headers(), path);
        }
        response.bytes().await.expect("body reads").to_vec()
    };

    for attempt in 1..=ATTEMPTS {
        let before = get(GO).await;
        let ours = get(RUST).await;
        let after = get(GO).await;

        if before == after {
            return (before, ours);
        }

        tokio::time::sleep(std::time::Duration::from_millis(50 * attempt)).await;
    }

    panic!(
        "{path}: Go's answer never settled over {ATTEMPTS} attempts, so no comparison here would mean anything"
    );
}

/// Remove every row the API-level fixtures create, by name prefix.
///
/// Purges at the **start** of a test, not the end: an assertion panics past any trailing cleanup,
/// and the first run to do so otherwise poisons every later one. That is not hypothetical here —
/// Go's `DELETE /api/v4/channels/{id}` **archives** rather than removes, and an archived channel
/// keeps its name, so the next run's create fails with `save_channel.exists`. The same is true of
/// a soft-deleted user's username. The HTTP API cannot undo either, which is why this reaches for
/// the database that both servers share.
/// Once per test binary. The harness runs tests concurrently, so a purge-per-test would delete
/// another test's in-flight fixtures — which it did: the parallel run left five channels behind
/// where the sequential one left one. `OnceCell` makes every later caller wait for the first
/// purge rather than start its own, so the clearing happens strictly before any test creates
/// anything.
static PURGED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

pub async fn purge_api_fixtures() {
    PURGED.get_or_init(purge_api_fixtures_once).await;
}

async fn purge_api_fixtures_once() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
    else {
        return;
    };

    for statement in [
        "DELETE FROM channelmembers WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%')",
        "DELETE FROM channelmembers WHERE userid IN (SELECT id FROM users WHERE username LIKE 'mmrsplain%')",
        "DELETE FROM teammembers WHERE userid IN (SELECT id FROM users WHERE username LIKE 'mmrsplain%')",
        "DELETE FROM sessions WHERE userid IN (SELECT id FROM users WHERE username LIKE 'mmrsplain%')",
        // Before the posts they attach to, while the channel subquery still resolves either way.
        "DELETE FROM fileinfo WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%')",
        "DELETE FROM posts WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%')",
        // `PublicChannels` is Go's denormalised shadow of the public-channel metadata, kept in
        // step by `upsertPublicChannelT`. It has its own `(Name, TeamId)` uniqueness, so a
        // leftover row there fails the next create with a 500 rather than the
        // `save_channel.exists` a leftover `Channels` row gives. Two different symptoms, one
        // cause, and only the second one names the table.
        "DELETE FROM publicchannels WHERE name LIKE 'mmrs-parity-%'",
        "DELETE FROM sidebarchannels WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%')",
        "DELETE FROM channelmemberhistory WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%')",
        "DELETE FROM channels WHERE name LIKE 'mmrs-parity-%'",
        // Teams created by tests: Go's `DELETE /teams/{id}` archives like the channel one, and
        // an archived team keeps its name, so the next run's create fails without this.
        "DELETE FROM teammembers WHERE teamid IN (SELECT id FROM teams WHERE name LIKE 'mmrs-parity-%')",
        "DELETE FROM teams WHERE name LIKE 'mmrs-parity-%'",
        // Rows the getUser suite plants directly (Team Edition cannot author a ToS over REST).
        "DELETE FROM usertermsofservice WHERE userid IN (SELECT id FROM users WHERE username LIKE 'mmrsplain%')",
        "DELETE FROM users WHERE username LIKE 'mmrsplain%'",
        // DMs opened with a deleted `mmrsplain` user. A DM is named `<id>__<id>` and carries no
        // `mmrs-parity-` prefix, so nothing above reaches it, and the fixture user had
        // accumulated 124 of them — all with an empty display name, all tied under the channel
        // list's `ORDER BY DisplayName`, and Go and Postgres broke the tie differently often
        // enough to fail the byte-for-byte assertion one run in three. Must run **after** the
        // users are gone, because "a side that names no user" is the selector.
        "DELETE FROM channelmembers WHERE channelid IN (SELECT id FROM channels WHERE type = 'D' AND (split_part(name, '__', 1) NOT IN (SELECT id FROM users) OR split_part(name, '__', 2) NOT IN (SELECT id FROM users)))",
        "DELETE FROM posts WHERE channelid IN (SELECT id FROM channels WHERE type = 'D' AND (split_part(name, '__', 1) NOT IN (SELECT id FROM users) OR split_part(name, '__', 2) NOT IN (SELECT id FROM users)))",
        "DELETE FROM sidebarchannels WHERE channelid IN (SELECT id FROM channels WHERE type = 'D' AND (split_part(name, '__', 1) NOT IN (SELECT id FROM users) OR split_part(name, '__', 2) NOT IN (SELECT id FROM users)))",
        "DELETE FROM channelmemberhistory WHERE channelid IN (SELECT id FROM channels WHERE type = 'D' AND (split_part(name, '__', 1) NOT IN (SELECT id FROM users) OR split_part(name, '__', 2) NOT IN (SELECT id FROM users)))",
        "DELETE FROM channels WHERE type = 'D' AND (split_part(name, '__', 1) NOT IN (SELECT id FROM users) OR split_part(name, '__', 2) NOT IN (SELECT id FROM users))",
    ] {
        let _ = sqlx::query(statement).execute(&pool).await;
    }
}

/// POST `body` (raw bytes, `Content-Type: application/json`) to a path on both servers with the
/// same token, returning `(status, body)` from each — the POST counterpart of [`fetch_both_raw`].
///
/// Raw bytes rather than a `serde_json::Value` on purpose: the status-ids route's parse branches
/// are about bodies that are *not* well-formed JSON, and a typed body could not express them.
pub async fn post_both_raw(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let post = async |base: &str| {
        let response = client
            .post(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        if base == RUST {
            assert_served_by_rust(response.headers(), path);
        }
        (status, response.bytes().await.expect("body reads").to_vec())
    };

    (post(GO).await, post(RUST).await)
}

/// Set a user's status through Go's `PUT /users/{id}/status`, returning Go's response body.
///
/// This is the one REST write that lands in **both** Go's status cache and the `Status` table
/// (`SaveAndBroadcastStatus`), which is what makes a status fixture comparable: the ported
/// routes read the table, Go reads the cache first, and they agree only where both were written.
/// Go answers the PUT with `getUserStatus`'s own body, so the return value doubles as an oracle.
pub async fn set_user_status(
    client: &reqwest::Client,
    token: &str,
    user_id: &str,
    status: &str,
    dnd_end_time: i64,
) -> Vec<u8> {
    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/status"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "user_id": user_id,
            "status": status,
            "dnd_end_time": dnd_end_time,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "setting {user_id} to {status} failed: {}",
        response.text().await.unwrap_or_default()
    );
    response.bytes().await.expect("body reads").to_vec()
}
