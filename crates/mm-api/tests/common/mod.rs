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
            // **The purge runs here, and this is the only place it can run safely.**
            //
            // `purge_api_fixtures` deletes by shared prefix — every `mmrsplain%` user's channel
            // and team memberships among them — and its own comment asks for it to happen
            // "before any fixture is built". A `OnceCell` on the purge alone cannot deliver that:
            // whichever test trips it first runs the sweep *while other suites already have
            // fixtures up*, and their rows go with it. Measured twice in a row —
            // `channel_members_list::pages_split_cover_and_run_out_identically` lost two of its
            // four members to a purge triggered by another suite's `create_plain_user`, and
            // `threads_for_user` has failed the same way.
            //
            // Nesting it inside the token's `OnceCell` fixes it, because **no stack-backed test
            // can build anything before it has a token**: they all await this cell, and the first
            // caller holds every other one here until the sweep and the login are both done.
            // Later `purge_api_fixtures()` calls become no-ops on an already-initialised cell,
            // so the existing call sites keep working and keep documenting their intent.
            purge_api_fixtures().await;

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
pub fn assert_served_by_rust(headers: &reqwest::header::HeaderMap, path: &str) {
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

/// **`Systems.ActiveLicenseId` is one row for the whole installation**, and more than one suite
/// writes it: every route that answers only for an unlicensed server proves its boundary by
/// planting a licence id and checking that the request is forwarded instead.
///
/// A read/write lock rather than a mutex, and shared here rather than per module: everything that
/// expects an unlicensed answer holds it **shared** and still runs in parallel; the tests that
/// make the server look licensed hold it **exclusively**, across suites. Two modules with their
/// own locks would not exclude each other, and the symptom would be a neighbouring suite finding
/// `x-mmrs-served-by: go` where it asserted `rust`.
pub static ACTIVE_LICENCE_ROW: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// **The shared admin's broadcast stream is one resource, and counting frames on it is exclusive.**
///
/// A websocket sees everything the server publishes to that connection, so a test asserting
/// "exactly one `user_updated`" or "exactly one `preferences_changed`" is really asserting that
/// *nothing else in the binary* touched the shared admin while its socket was open. Scoping by
/// subject is not enough when the other writer is a sibling test changing the same admin, and
/// scoping by payload is not enough when both write the same category.
///
/// This lock is what makes those counts true. Every test that opens a `SocketProbe` and asserts a
/// **count** holds it for the whole exchange — connect, write, collect, assert.
///
/// It was not needed while the Go server ran under qemu: the tests were slow enough to miss each
/// other. Replacing that image with a native build of the pinned source made the suite roughly six
/// times faster and turned four of these into failures on unchanged code. The races were always
/// there; the emulator was hiding them.
pub static BROADCAST_STREAM: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Write `Systems.ActiveLicenseId`, or clear it when `id` is `None`.
///
/// A 26-character value passes `IsValidId`, which is all `LoadLicense` checks before it looks the
/// licence up — so the row alone is enough to make this side believe the installation is licensed.
/// **Go is unmoved by it**: it loaded its licence at startup and re-reads only on a save, so its
/// answers do not change and the observable difference is which server produced them.
pub async fn set_active_licence_id(id: Option<&str>) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");

    sqlx::query("DELETE FROM systems WHERE name = 'ActiveLicenseId'")
        .execute(&pool)
        .await
        .expect("the active licence id is cleared");

    if let Some(id) = id {
        assert_eq!(id.len(), 26, "`IsValidId` requires 26 characters");
        sqlx::query("INSERT INTO systems (name, value) VALUES ('ActiveLicenseId', $1)")
            .bind(id)
            .execute(&pool)
            .await
            .expect("the active licence id is written");
    }
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
        // The *set*, not the order: both sides are `serde_json::Value` objects, which are
        // `BTreeMap`s without the `preserve_order` feature, so each side comes back alphabetical
        // whatever its bytes said. Error bodies are compared by field here and by bytes nowhere,
        // so that is the right claim — but it is not the claim this message used to make.
        "{context}: the two bodies must carry the same set of keys"
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
    // The sweep in [`purge_api_fixtures`] is what removes an earlier run's `mmrsplain%` rows, and
    // a create here fails outright without it: an assertion panics past the trailing
    // [`delete_plain_user`], so an aborted run leaves the username taken and Go answers
    // `app.user.save.username_exists.app_error` to every later run. Suites that only *use* a plain
    // user had no other reason to purge, so the guarantee belongs here rather than at each call
    // site. It is a `OnceCell`, so this costs nothing after the first caller.
    purge_api_fixtures().await;

    let username = plain_username(tag);
    let password = PLAIN_USER_PASSWORD;

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

    PlainUser {
        id,
        token: login_plain_user(client, tag).await,
    }
}

/// The password [`create_plain_user`] sets. Exposed so a fixture can log the same user in again.
pub const PLAIN_USER_PASSWORD: &str = "Mmrs-Plain-1234";

/// The username [`create_plain_user`] derives from a tag.
pub fn plain_username(tag: &str) -> String {
    format!("mmrsplain{tag}")
}

/// Log a plain user in again, returning a **fresh** token.
///
/// # Why a role change needs this
///
/// `SessionHasPermissionTo` reads `session.Roles` (web/context.go), not `Users.Roles` — the roles
/// are copied onto the session row at login and never re-read. So granting a role, by SQL *or*
/// through `PUT /users/{id}/roles`, leaves every existing token holding the old set, and a fixture
/// that grants `system_read_only_admin` and reuses its token gets a 403 from **Go**. Measured, in
/// the 2026-09-07 run; the test looked like a port bug and was a fixture bug.
pub async fn login_plain_user(client: &reqwest::Client, tag: &str) -> String {
    let login = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({
            "login_id": plain_username(tag),
            "password": PLAIN_USER_PASSWORD,
        }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(login.status(), 200, "the plain user cannot log in");
    login
        .headers()
        .get("token")
        .expect("Go returns a token header")
        .to_str()
        .expect("ASCII")
        .to_owned()
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
    create_channel_typed(client, admin_token, team_id, tag, "O").await
}

/// [`create_channel`] with the type spelled out.
///
/// **A public channel cannot test a refusal.** `HasPermissionToReadChannel` falls back to
/// `read_public_channel` on the *team* for an open channel, and `create_plain_user` puts its user
/// in the team — so a "non-member" is served, not refused, and an assertion expecting a 403 fails
/// against Go. Measured: five tests in this suite were written that way and Go answered 200 to
/// every one. Anything asserting that membership is what grants access needs `"P"`.
pub async fn create_channel_typed(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    tag: &str,
    channel_type: &str,
) -> String {
    // The purge precedes every fixture write — see [`create_team`].
    purge_api_fixtures().await;

    let name = format!("mmrs-parity-{tag}");
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": name,
            "display_name": format!("mmrs parity {tag}"),
            "type": channel_type,
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
/// `UrgentMentionCount` or `LastViewedAt`, so the only way to exercise the coalesce — and to catch
/// its removal — is to write the NULL directly. Returns `false` when `DATABASE_URL` is unset, so
/// the caller can skip rather than fail.
pub async fn null_out_member_column(channel_id: &str, user_id: &str, column: &str) -> bool {
    assert!(
        matches!(column, "urgentmentioncount" | "lastviewedat"),
        "only the coalesced columns are allowed here; widening this needs a reason"
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
    // The column name is from the closed set asserted above, so the format is not an injection
    // point; the two ids are bound.
    let statement =
        format!("UPDATE channelmembers SET {column} = NULL WHERE channelid = $1 AND userid = $2");
    sqlx::query(&statement)
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
/// So this reads **Go, then Rust, then Go again** and accepts our answer when it equals *either*
/// Go read. The returned Go body is whichever one matched, so a caller's byte comparison means
/// what it always meant.
///
/// # Why "matches either" and not "wait until Go stops moving"
///
/// The original version required Go's two reads to be identical and retried until they were.
/// That works for a row settling after one write and fails for a list that is genuinely growing:
/// `/users/me/channels` changes for as long as *any* suite in this binary is building fixtures,
/// because Go joins a channel's creator to it and every suite creates channels. The budget was
/// raised twice — 8 → 12 → 20 attempts — and the list still never went quiet for a whole read
/// triple, so the test failed with "never settled" while both servers were perfectly agreed.
///
/// Bracketing is the stronger check anyway. A **correct** port answers something Go also
/// answered at some instant inside the window, so it matches one of the two. A **wrong** port
/// matches neither, whatever the list is doing — the divergence does not hide behind churn,
/// which is what the quiescence version was really trying to arrange.
///
/// The quiescence check is **kept as a third acceptance**, because both brackets compare bytes:
/// a route whose element order is Go's heap order (`/users/me/teams/members`) fails them on
/// ordering alone on every read, and its own assertion — which normalises that order — never gets
/// to run. So: match a bracket, or find Go still, or retry.
pub async fn fetch_both_stable(
    client: &reqwest::Client,
    token: &str,
    path: &str,
) -> (Vec<u8>, Vec<u8>) {
    fetch_both_stable_within(client, token, path, 24).await
}

/// [`fetch_both_stable`] with the retry budget spelled out.
///
/// The default is **24**, raised from twelve when the schemes suite landed: it creates four users,
/// five teams and two channels in one fixture, and every one of those is a row in the user list
/// and two rows in the admin's audit page. Two long-standing tests started failing on churn alone
/// — `users_list::the_unfiltered_list_matches_go` and `user_audits::me_resolves_to_the_caller` —
/// neither of which had anything to do with the routes being added.
///
/// The backoff is capped so a *real* divergence still fails quickly: without a cap, doubling the
/// attempts would have quadrupled the time a genuinely broken route takes to report itself.
///
/// It is still not enough for `GET /api/v4/audits`, whose page 0 shifts on every login anywhere in
/// this binary; that one compares a shifted window instead. Measured: a no-op control mutation was
/// reported CAUGHT because this call exhausted its budget, which is the harness lying about a
/// verdict rather than a port being wrong.
pub async fn fetch_both_stable_within(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    attempts: u64,
) -> (Vec<u8>, Vec<u8>) {
    let max_attempts = attempts;

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

    let mut last = (Vec::new(), Vec::new(), Vec::new());
    for attempt in 1..=max_attempts {
        let before = get(GO).await;
        let ours = get(RUST).await;
        let after = get(GO).await;

        if ours == before {
            return (before, ours);
        }
        if ours == after {
            return (after, ours);
        }
        // Go quiescent across the window: whatever is left is ours to explain, so hand the pair
        // over and let the caller's own assertion — which may normalise element order, as
        // `team_members_route` does — decide. This is the original quiescence check, kept
        // because the two bracket tests above compare **bytes**: a route whose element order is
        // heap order fails both of them on ordering alone, every time, and would never get here
        // without this line.
        if before == after {
            return (before, ours);
        }

        last = (before, ours, after);
        tokio::time::sleep(std::time::Duration::from_millis((80 * attempt).min(400))).await;
    }

    // Matching neither bracket, repeatedly, is a divergence rather than churn — so fail with the
    // comparison the caller wanted rather than with a note about the fixture.
    let (before, ours, _after) = last;
    (before, ours)
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
///
/// **"Before any test creates anything" is only true if every creation path awaits it**, and that
/// is now enforced: [`create_team`], [`create_channel_typed`] and [`create_plain_user`] all call
/// this first. Until they did, a suite could create a team, and a *later* first-caller elsewhere
/// would then run the purge and delete it — surfacing as an unrelated suite failing to add a user
/// to a channel whose team had lost its members.
static PURGED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Delete every row the api suites author, once per test binary.
///
/// # [D-155], closed
///
/// Selection is by the `mmrs-parity-%` name prefix, which does not reach the rows *Go* authors
/// on a fixture's behalf: a created team's `town-square` and `off-topic` carry no prefix, and
/// the `teams` delete below orphans both. An orphaned channel's dangling `TeamId` arrives as
/// NULL through the channel-member join and is therefore listed under **every** team, and its
/// empty display name ties under the channel lists' `ORDER BY DisplayName`.
///
/// The note here used to end "delete by `TeamId`-has-no-team, not by name, when this is fixed".
/// That is what the orphan sweep at the end of this function now does — and it was not
/// cosmetic: the development database had reached **16,066** orphaned channels against 25 live
/// ones, with 50,000-odd posts hanging off them, and three different suites failed one run each
/// on ties and cross-team leakage that all trace back here.
///
/// The sweep is deliberately **not** limited to this project's names. An orphan is defined by
/// its dangling `TeamId`, exactly as the old note asked: a channel whose team does not exist is
/// unreachable through any API on either server, so nothing that deletes it can be observed by a
/// test. DMs and GMs carry `TeamId = ''` and are excluded by construction.
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
        // Rows keyed on a *post* id, which nothing below reaches — the channel subquery is the
        // only handle on them, and it stops resolving once the posts are gone. These used to
        // live in each suite's own purge, which is a race rather than a cleanup: the parity
        // tests share one binary and one database, so a purge running inside suite A's fixture
        // deletes suite B's rows if B built its fixture first. Measured — `emoji_get` and
        // `post_get` each dropped the other's custom emoji, one run in two. Anything that
        // deletes by a shared prefix belongs here, in the `OnceCell` that runs before any
        // fixture is built.
        // Flagged-post preferences. These are keyed on a *post* id with no prefix to select on,
        // and they are written against the shared admin user as well as the `mmrsplain%` ones —
        // so a leftover flag from a previous run, or from a hand-run probe, silently joins the
        // next run's flagged-post list and breaks any assertion about its contents. Deleting the
        // whole category is safe because `flagged_post` is authored by exactly one suite.
        "DELETE FROM preferences WHERE category = 'flagged_post'",
        // Webhooks named by the parity suite. **These must be swept even though every test
        // deletes its own**, because the outgoing store's `DeleteAt` is a *soft* delete and the
        // intersection check `CreateOutgoingWebhook` runs carries **no `DeleteAt` predicate**
        // (webhook.go) — so a deleted hook keeps its trigger words and callback URLs reserved
        // for ever. The second run of `webhook_writes` failed against the first run's rows.
        // OAuth apps the suite creates. `OAuthApps` has no `DeleteAt`, so an aborted run leaves
        // rows that later show up in every list read.
        "DELETE FROM oauthaccessdata WHERE clientid IN (SELECT id FROM oauthapps WHERE name LIKE 'mmrs%')",
        "DELETE FROM preferences WHERE category = 'oauth_app' AND name IN (SELECT id FROM oauthapps WHERE name LIKE 'mmrs%')",
        "DELETE FROM oauthapps WHERE name LIKE 'mmrs%'",
        "DELETE FROM incomingwebhooks WHERE displayname LIKE 'mmrs%'",
        "DELETE FROM outgoingwebhooks WHERE displayname LIKE 'mmrs%'",
        "DELETE FROM reactions WHERE emojiname LIKE 'mmrsparity%'",
        "DELETE FROM reactions WHERE postid IN (SELECT id FROM posts WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%'))",
        "DELETE FROM postspriority WHERE postid IN (SELECT id FROM posts WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%'))",
        "DELETE FROM postacknowledgements WHERE postid IN (SELECT id FROM posts WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrs-parity-%'))",
        // Schemes planted by `plant_scheme`. **The detach comes first**: `Teams.SchemeId` and
        // `Channels.SchemeId` are plain columns with no foreign key, so deleting the scheme row
        // first would leave a team pointing at a scheme that no longer exists — which Go reads as
        // a scheme-less team on some paths and errors on others. Detach, then delete.
        "UPDATE teams SET schemeid = NULL WHERE schemeid LIKE 'mmrsscheme%'",
        "UPDATE channels SET schemeid = NULL WHERE schemeid LIKE 'mmrsscheme%'",
        "DELETE FROM schemes WHERE id LIKE 'mmrsscheme%'",
        // Roles planted by `plant_role`. **The users first**: `Users.Roles` is a space-separated
        // string with no foreign key, so a leftover `mmrs_role_x` there outlives the role row and
        // is silently skipped by every permission check — which reads as a permission the fixture
        // thought it had granted.
        "UPDATE users SET roles = 'system_user' WHERE roles LIKE '%mmrs_role_%'",
        "DELETE FROM roles WHERE name LIKE 'mmrs_role_%'",
        // Bots and their owners planted by `mm-store`'s `db_bot_store` test. They live in a
        // different binary, but they land in the *same* database and `Users` is shared: a run
        // that panicked past that file's own cleanup leaves rows that `users_stats` counts and
        // `bots` compares. Cheap to sweep, and the alternative is an unexplained off-by-two.
        "DELETE FROM bots WHERE userid LIKE 'mmrsbot%'",
        "DELETE FROM users WHERE id LIKE 'mmrsbot%'",
        // Go's DELETE on an emoji is a **soft** delete and the name stays taken, so the row has
        // to go or the next run cannot create one.
        "DELETE FROM emoji WHERE name LIKE 'mmrsparity%'",
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
        // ---- [D-155]: channels whose team no longer exists, and everything hanging off them.
        //
        // Dependents first, each selected through the same orphan predicate, because the
        // channel delete is what makes them unreachable rather than what removes them. The
        // predicate is `TeamId` names no `Teams` row — never a name prefix, so it collects the
        // `town-square` and `off-topic` Go creates for a team this suite later deletes.
        "DELETE FROM threadmemberships WHERE postid IN (SELECT p.id FROM posts p JOIN channels c ON c.id = p.channelid WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid))",
        "DELETE FROM threads WHERE postid IN (SELECT p.id FROM posts p JOIN channels c ON c.id = p.channelid WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid))",
        "DELETE FROM postspriority WHERE postid IN (SELECT p.id FROM posts p JOIN channels c ON c.id = p.channelid WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid))",
        "DELETE FROM postacknowledgements WHERE postid IN (SELECT p.id FROM posts p JOIN channels c ON c.id = p.channelid WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid))",
        "DELETE FROM reactions WHERE postid IN (SELECT p.id FROM posts p JOIN channels c ON c.id = p.channelid WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid))",
        "DELETE FROM fileinfo WHERE channelid IN (SELECT c.id FROM channels c WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid))",
        "DELETE FROM posts WHERE channelid IN (SELECT c.id FROM channels c WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid))",
        "DELETE FROM channelmembers WHERE channelid IN (SELECT c.id FROM channels c WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid))",
        "DELETE FROM channelmemberhistory WHERE channelid IN (SELECT c.id FROM channels c WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid))",
        "DELETE FROM sidebarchannels WHERE channelid IN (SELECT c.id FROM channels c WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid))",
        "DELETE FROM publicchannels WHERE teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = publicchannels.teamid)",
        "DELETE FROM channels c WHERE c.teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = c.teamid)",
        // `SidebarCategories` are keyed on `TeamId` and outlive their team the same way.
        "DELETE FROM sidebarcategories WHERE teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = sidebarcategories.teamid)",
        // ---- The same rule, one reference further out. These run **last** because the deletes
        // above are what strand them: a `Threads` row survives its root post, and a `Posts` row
        // survives its channel, and neither is reachable through any API afterwards. Left alone
        // they are the largest unbounded growth in the fixture database — 3,190 dangling thread
        // rows against 4 live ones when this sweep was written.
        "DELETE FROM posts WHERE NOT EXISTS (SELECT 1 FROM channels c WHERE c.id = posts.channelid)",
        // Hooks whose team or channel is gone. The suite creates teams and channels per test and
        // deletes them; a hook outliving its team is invisible to every REST route and still
        // reserves its trigger words.
        "DELETE FROM outgoingwebhooks WHERE teamid <> '' AND NOT EXISTS (SELECT 1 FROM teams t WHERE t.id = outgoingwebhooks.teamid)",
        "DELETE FROM incomingwebhooks WHERE channelid <> '' AND NOT EXISTS (SELECT 1 FROM channels c WHERE c.id = incomingwebhooks.channelid)",
        "DELETE FROM threadmemberships WHERE NOT EXISTS (SELECT 1 FROM posts p WHERE p.id = threadmemberships.postid)",
        "DELETE FROM threads WHERE NOT EXISTS (SELECT 1 FROM posts p WHERE p.id = threads.postid)",
        // A `Drafts` row survives its channel the same way, and no API can reach it afterwards:
        // `getDrafts` inner-joins `ChannelMembers`, so an orphan is invisible to the route that
        // would otherwise clean it up. Nine rows against two live ones when the drafts suite was
        // written, from three days of runs.
        "DELETE FROM drafts WHERE NOT EXISTS (SELECT 1 FROM channels c WHERE c.id = drafts.channelid)",
        "DELETE FROM drafts WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.id = drafts.userid)",
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

/// [`post_both_raw`] with the same quiescence bracket [`fetch_both_stable_within`] uses.
///
/// A single Go-then-Rust pair compares two reads taken at different instants, so any row the
/// answer embeds and another suite writes shows up as a byte difference that is not a divergence.
/// `POST /users/group_channels` embeds whole `User` objects, and the admin's `Users.UpdateAt` is
/// touched by half the suites in this binary — so the pair fails on churn alone, intermittently,
/// with a 4KB byte-array diff that says nothing about the route.
///
/// The bracket is Go, us, Go: if our body matches either Go read, that is the answer; if Go was
/// quiescent across the window and we still differ, it is ours to explain and the caller's
/// assertion decides. Same three-way rule, same reason.
pub async fn post_both_raw_stable(
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

    let mut last = None;
    for attempt in 1..=12u64 {
        let before = post(GO).await;
        let ours = post(RUST).await;
        let after = post(GO).await;

        if ours == before {
            return (before, ours);
        }
        if ours == after {
            return (after, ours);
        }
        if before == after {
            return (before, ours);
        }

        last = Some((before, ours));
        tokio::time::sleep(std::time::Duration::from_millis((80 * attempt).min(400))).await;
    }

    // Matching neither bracket, repeatedly, is a divergence rather than churn.
    last.expect("the loop runs at least once")
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

/// A 1x1 PNG, small enough to inline and real enough for Go's image decoder — which both the
/// emoji endpoint and the file uploader run before accepting the bytes.
pub const TINY_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0,
    0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 0, 3, 1,
    1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// `POST /api/v4/emoji` takes multipart and nothing else, so the body is assembled by hand
/// rather than by pulling reqwest's `multipart` feature — and a Cargo feature change — into the
/// tree for one call. Returns the new emoji's id.
///
/// **Name the emoji uniquely per run.** `LocalCacheEmojiStore` memoises `GetByName` for thirty
/// minutes, and the SQL purges these suites run delete the row straight from Postgres, which the
/// Go server never hears about. A reused name then fails the next create with
/// `api.emoji.create.duplicate.app_error` against a row that no longer exists — measured, not
/// theorised. [`unique_emoji_name`] is the tag-plus-timestamp form the suites use.
pub async fn create_custom_emoji(
    client: &reqwest::Client,
    token: &str,
    creator_id: &str,
    name: &str,
) -> String {
    const BOUNDARY: &str = "mmrsparityemojiboundary";
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"image\"; filename=\"e.png\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(TINY_PNG);
    body.extend_from_slice(
        format!(
            "\r\n--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"emoji\"\r\n\r\n\
             {{\"name\":\"{name}\",\"creator_id\":\"{creator_id}\"}}\r\n--{BOUNDARY}--\r\n"
        )
        .as_bytes(),
    );

    let response = client
        .post(format!("{GO}/api/v4/emoji"))
        .header("Authorization", format!("Bearer {token}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture emoji failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the emoji decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// `mmrsparity<tag><millis>` — the `mmrsparity` prefix is what the SQL purges collect on, and
/// the timestamp is what keeps Go's thirty-minute name cache from rejecting the next run. Only
/// lower-case letters and digits, so it is inside both the emoji-name validator and the mux.
pub fn unique_emoji_name(tag: &str) -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    format!("mmrsparity{tag}{stamp}")
}

/// Go's emoji delete is a **soft** delete — it sets `DeleteAt` and leaves the row and its name.
pub async fn delete_custom_emoji(client: &reqwest::Client, token: &str, emoji_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/emoji/{emoji_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "deleting the fixture emoji failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Upload `bytes` to `channel_id` through Go's simple (non-multipart) upload path and return the
/// new `FileInfo`'s id.
///
/// `POST /api/v4/files?channel_id=&filename=` with the file in the body is `uploadFileSimple`
/// (api4/file.go:130) — no multipart assembly needed, unlike [`create_custom_emoji`].
///
/// The file is uploaded but **not attached**: `PostId` stays empty until a post claims it, which
/// is what [`post_message_with_files`] does. That gap is itself a fixture — a `FileInfo` with no
/// post is what makes `getFileInfo`'s empty-`ChannelId` branch reachable.
pub async fn upload_file(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> String {
    let response = client
        .post(format!(
            "{GO}/api/v4/files?channel_id={channel_id}&filename={filename}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", content_type)
        .body(bytes.to_vec())
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "uploading {filename} to {channel_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
    let uploaded: serde_json::Value = response.json().await.expect("the upload decodes");
    uploaded["file_infos"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned()
}

/// Post to `channel_id` with `file_ids` attached, returning the new post's id.
///
/// Deliberately separate from [`post_message`] rather than another `Option` parameter on it:
/// `Post.PreSave` **sorts and deduplicates** `FileIds` (post.go:740), so the order a test sends
/// is not the order the column holds, and a caller needs to be looking at that when it writes
/// an ordering assertion.
pub async fn post_message_with_files(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    message: &str,
    file_ids: &[String],
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel_id,
            "message": message,
            "file_ids": file_ids,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "posting with files to {channel_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the post decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Pin `post_id` — `POST /api/v4/posts/{post_id}/pin`.
pub async fn pin_post(client: &reqwest::Client, token: &str, post_id: &str) {
    let response = client
        .post(format!("{GO}/api/v4/posts/{post_id}/pin"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "pinning {post_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Write one `FileInfo` column straight through the shared database.
///
/// Restricted to the three columns nothing in the REST API can set, each of which gates a branch
/// that would otherwise be untestable:
///
/// - `archived` — `Save` does not list it among its INSERT columns (file_info_store.go:113) and
///   the only writer of `true` is `MakeContentInaccessible`, which never reaches Postgres. It is
///   how the `GetByIds`-drops-it-and-`Get`-keeps-it divergence is observed at all.
/// - `minipreview` — the upload path generates one for every image it accepts, so a NULL on an
///   image row is what sends both file routes down the forward path.
/// - `deleteat` — `DELETE /files/{id}` does not exist; a file is only soft-deleted as a side
///   effect of deleting its post, which would take the post with it.
/// - `channelid` — the column is nullable because it was added after `FileInfo` existed, and the
///   upload path has filled it in ever since (app/file.go:774). A NULL here is what a
///   pre-migration row looks like, and it is the only way to reach `getFileInfo`'s
///   `GetChannel("")` branch.
///
/// Returns `false` when `DATABASE_URL` is unset so the caller can skip rather than fail.
pub async fn set_fileinfo_column(file_id: &str, column: &str, value: &str) -> bool {
    assert!(
        matches!(
            column,
            "archived" | "minipreview" | "deleteat" | "channelid"
        ),
        "only the three columns the REST API cannot reach are allowed here; widening this needs a reason"
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
    // The column name is from the closed set asserted above, so the format is not an injection
    // point; the *value* is bound. `deleteat` and `archived` both take a literal here, which is
    // why `value` is a `&str` and not a typed parameter.
    let statement = format!("UPDATE fileinfo SET {column} = {value} WHERE id = $1");
    sqlx::query(&statement)
        .bind(file_id)
        .execute(&pool)
        .await
        .expect("the fixture file's column is written");
    true
}

/// Soft-delete a post — `DELETE /api/v4/posts/{post_id}`, which sets `DeleteAt` and leaves the
/// row. Deleting a **root** takes its replies with it, so a fixture that wants one deleted reply
/// must delete the reply and not the thread.
pub async fn delete_post(client: &reqwest::Client, token: &str, post_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "deleting {post_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Set a user's timezone through Go's `PUT /users/{id}/patch`.
///
/// `use_automatic` is the string `"true"`/`"false"` Go stores, not a bool: `Timezone` is a
/// `model.StringMap`, and `GetPreferredTimezone` compares it against the literal `"true"`.
pub async fn patch_user_timezone(
    client: &reqwest::Client,
    token: &str,
    user_id: &str,
    use_automatic: &str,
    automatic: &str,
    manual: &str,
) {
    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "timezone": {
                "useAutomaticTimezone": use_automatic,
                "automaticTimezone": automatic,
                "manualTimezone": manual,
            }
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "setting {user_id}'s timezone failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Edit a post through Go's `PUT /posts/{id}`, which is what writes an edit-history row.
///
/// Go stores the **old** version as a new row carrying `OriginalId = <the live post's id>`, so
/// each call adds one entry to the history rather than replacing it.
pub async fn update_post(client: &reqwest::Client, token: &str, post_id: &str, message: &str) {
    let response = client
        .put(format!("{GO}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "id": post_id, "message": message }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "editing {post_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Plant a `UserTermsOfService` row straight into the shared database — Team Edition cannot
/// author a terms of service over REST, so this is the only way to make the branch's found case
/// reachable. Both servers read the same row; `purge_api_fixtures` clears it with its user.
///
/// Lives here rather than in one suite because two of them need it — `user_get`, which reads the
/// row through a user body, and `user_terms_of_service`, which reads it through its own route.
pub async fn plant_terms_of_service_row(user_id: &str, tos_id: &str) -> bool {
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
        "INSERT INTO usertermsofservice (userid, termsofserviceid, createat)
         VALUES ($1, $2, 1700000000000)
         ON CONFLICT (userid) DO UPDATE SET termsofserviceid = $2, createat = 1700000000000",
    )
    .bind(user_id)
    .bind(tos_id)
    .execute(&pool)
    .await
    .expect("plants the terms-of-service row");
    true
}

/// Count the users `/api/v4/users/stats` claims to count, straight from the shared database.
///
/// An independent oracle for the route's two predicates: `DeleteAt = 0` and the nullable-or-empty
/// `RemoteId`. Deliberately **not** an anti-join against `Bots` — `IncludeBotAccounts` is `true`
/// at the one call site, so a bot is a user for this purpose, and asserting that against a query
/// written the other way is how the flag gets tested at all.
///
/// Returns `None` when `DATABASE_URL` is unset, so the caller can skip rather than fail.
pub async fn count_countable_users() -> Option<i64> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .ok()?;
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM users WHERE deleteat = 0 AND (remoteid = '' OR remoteid IS NULL)",
    )
    .fetch_one(&pool)
    .await
    .ok()
}

/// Overwrite one user's `Roles` column directly.
///
/// The only way to reach a code path gated on **not** holding a permission every real account
/// has: `system_user` grants `view_members` outright, so `GetViewUsersRestrictions` returns nil
/// for everybody a REST call can create. Writing a role name that no `Roles` row defines makes
/// `RolesGrantPermission` answer false for that one account and nothing else.
///
/// Per-user by construction, so it cannot disturb a concurrently running suite — unlike editing
/// the `system_user` role itself, which is global and would.
pub async fn set_user_roles(user_id: &str, roles: &str) -> bool {
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
    sqlx::query("UPDATE users SET roles = $2 WHERE id = $1")
        .bind(user_id)
        .bind(roles)
        .execute(&pool)
        .await
        .expect("the fixture user's roles are written");
    true
}

/// Create an open team through Go's API and return its id.
///
/// The three suites that needed one each carried their own byte-identical copy of this; it moved
/// here when a fourth wanted it. A team of its own is what a fixture reaches for when the shared
/// one is too crowded to prove a negative — joining a team auto-joins `town-square`, so every
/// member of the shared fixture team already shares a channel with every other.
///
/// The `mmrs-parity-` prefix is what `purge_api_fixtures` collects on. Note [D-155]: the
/// `town-square` and `off-topic` Go creates alongside carry no prefix and are orphaned rather
/// than deleted.
pub async fn create_team(client: &reqwest::Client, admin_token: &str, tag: &str) -> String {
    // See [`purge_api_fixtures`]: **every** path that creates a fixture awaits the purge first, so
    // the purge is guaranteed to be the earliest write of the run. A suite that created a team and
    // only later triggered the purge — through some other suite's `create_plain_user`, say — had
    // its own team deleted out from under it, and the symptom was a *different* suite failing to
    // add a user to a channel on a team that no longer had members.
    purge_api_fixtures().await;

    let response = client
        .post(format!("{GO}/api/v4/teams"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture team failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the team decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Remove `user_id` from `team_id` through Go's API.
///
/// Go **soft-deletes**: the `TeamMembers` row survives with a non-zero `DeleteAt`, which is the
/// only way to build a departed member — the row has to still be there for a `DeleteAt = 0`
/// predicate to be worth asserting. Deleting the user instead would remove it from every answer
/// for a different reason and prove nothing about the predicate.
pub async fn remove_user_from_team(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    user_id: &str,
) {
    let response = client
        .delete(format!("{GO}/api/v4/teams/{team_id}/members/{user_id}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "removing {user_id} from {team_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Open a direct-message channel between two users and return its id.
///
/// A DM is a channel like any other for membership purposes, and it needs **no team** — which is
/// what makes it the way to give two users in different teams exactly one thing in common.
/// `users_known` relies on that: a team cannot be joined without also joining its `town-square`,
/// and Go refuses to remove anyone from a default channel, so "these two share nothing" is only
/// arrangeable across teams.
pub async fn create_direct_channel(
    client: &reqwest::Client,
    token: &str,
    user_a: &str,
    user_b: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([user_a, user_b]))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "opening a direct channel between {user_a} and {user_b} failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Clear the Go server's in-process caches through `POST /api/v4/caches/invalidate`.
///
/// # Why a parity test is allowed to do this
///
/// [D-087] says the Rust side never caches and is therefore never staler than Go. Most routes
/// that would expose the difference read a cache Go refreshes on write, so the window closes on
/// its own within a request or two. `GET /api/v4/usage/posts` does not: its count sits in a
/// **size-1, thirty-minute** cache (`localcachelayer/layer.go:342`) that nothing invalidates on a
/// new post, so a stack that has been up for a while answers with a number that can be minutes or
/// half an hour old. Measured on this deployment: Go said `400` where the table held `18`.
///
/// A byte comparison against that is not a test of the port — it is a test of when Go last
/// looked. Clearing the cache first makes the comparison about the query, which is the thing that
/// can actually be wrong.
///
/// # It is safe for the rest of the suite
///
/// Invalidation can only make Go **fresher**, and every other staleness assertion here is
/// one-sided in that direction — `users_me` asserts Go's `update_at` "can be stale but never
/// ahead of the row". A fresher Go satisfies all of them.
///
/// Requires a system-admin token. Panics if Go refuses, because a silently skipped invalidation
/// would turn this into a flake rather than a failure.
pub async fn invalidate_go_caches(client: &reqwest::Client, admin_token: &str) {
    let response = client
        .post(format!("{GO}/api/v4/caches/invalidate"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(
        response.status(),
        200,
        "cache invalidation needs a system admin: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Open a one-connection pool on `DATABASE_URL`, or [`None`] when the suite is running without
/// one.
///
/// Five helpers below plant rows no REST call can create. They each opened their own pool; this
/// is that, once.
pub(crate) async fn fixture_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()
}

/// Set `Teams.CloudLimitsArchived` directly.
///
/// **No REST route writes this column.** It is set by the cloud billing job when a team is
/// archived for exceeding a plan's team limit, and `GET /api/v4/usage/teams` reports the count of
/// such teams. Without planting one, the archived counter is zero on every read and three separate
/// mutations of its predicate survive the whole suite — measured, in the 2026-09-07 run.
pub async fn set_team_cloud_limits_archived(team_id: &str, archived: bool) -> bool {
    let Some(pool) = fixture_pool().await else {
        return false;
    };
    sqlx::query("UPDATE teams SET cloudlimitsarchived = $2 WHERE id = $1")
        .bind(team_id)
        .bind(archived)
        .execute(&pool)
        .await
        .expect("the fixture team's archived flag is written");
    true
}

/// Insert a post whose `Type` is set but is **not** a `system_` type, returning its id.
///
/// `UsersPostsOnly` is `Type = '' AND UserId NOT IN (SELECT UserId FROM Bots)`, while the
/// neighbouring `ExcludeSystemPosts` option is `Type NOT LIKE 'system_%'`. On a server whose posts
/// are all either untyped or `system_*`, those two predicates return the same count and a mutation
/// swapping them survives. No REST route creates a post with an arbitrary custom type — the API
/// rejects unknown types — so the row is planted directly.
///
/// The caller must delete it; leaving it behind would change the post count every other suite sees.
pub async fn plant_custom_typed_post(
    channel_id: &str,
    user_id: &str,
    post_type: &str,
) -> Option<String> {
    let pool = fixture_pool().await?;
    let id: String = format!("mmrscustomtype{:012}", rand_suffix());
    let now = now_millis();
    sqlx::query(
        "INSERT INTO posts (id, createat, updateat, deleteat, userid, channelid, rootid, \
         originalid, message, type, props, hashtags, filenames, fileids, hasreactions, editat, \
         ispinned, remoteid) \
         VALUES ($1, $2, $2, 0, $3, $4, '', '', 'planted by the parity suite', $5, '{}'::jsonb, \
         '', '[]', '[]', false, 0, false, NULL)",
    )
    .bind(&id)
    .bind(now)
    .bind(user_id)
    .bind(channel_id)
    .bind(post_type)
    .execute(&pool)
    .await
    .expect("the custom-typed post is written");
    Some(id)
}

/// Delete a row planted by [`plant_custom_typed_post`].
pub async fn delete_planted_post(post_id: &str) {
    let Some(pool) = fixture_pool().await else {
        return;
    };
    sqlx::query("DELETE FROM posts WHERE id = $1")
        .bind(post_id)
        .execute(&pool)
        .await
        .expect("the planted post is removed");
}

/// Read a `Systems` row, so a fixture can restore whatever was there.
///
/// Three states, and the fixture needs all three: no row at all, a row whose `Value` is SQL NULL,
/// and a row with text. The outer `Option` is "the suite has no database"; the middle one is
/// "no row"; the inner one is the nullable column.
pub async fn system_value(name: &str) -> Option<Option<Option<String>>> {
    let pool = fixture_pool().await?;
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT value FROM systems WHERE name = $1")
            .bind(name)
            .fetch_optional(&pool)
            .await
            .expect("the systems table is readable");
    Some(row.map(|row| row.0))
}

/// The `Status` **row**, which is not the same thing as the cached status either server answers
/// with.
///
/// Two of the fields that decide `SetStatusOnline`'s branches never reach the wire — `manual` is
/// on it but `prev_status` carries `json:"-"` — so the row is the only place a test can see them.
/// Returns `(status, manual, prev_status, dnd_end_time, last_activity_at)`.
/// The `Users.Props` map as the row holds it.
///
/// A custom status lives here as a JSON **string** under `customStatus`, and neither server's
/// REST surface exposes the prop directly — `SanitizeProfile` does not strip it, but a cleared
/// status is the empty string and a never-set one is an absent key, and only the row can tell
/// those apart.
pub async fn user_props(user_id: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let pool = fixture_pool().await?;
    let row: Option<(Option<serde_json::Value>,)> =
        sqlx::query_as("SELECT props FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&pool)
            .await
            .expect("the users table is readable");
    row.and_then(|row| row.0)
        .and_then(|value| value.as_object().cloned())
}

pub type StatusRow = (String, bool, String, i64, i64);

/// The nullable shape the columns actually have, before the defaults are folded in.
type NullableStatusRow = (
    Option<String>,
    Option<bool>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

pub async fn status_row(user_id: &str) -> Option<StatusRow> {
    let pool = fixture_pool().await?;
    // Every column but the primary key is nullable in the Go schema.
    let row: Option<NullableStatusRow> = sqlx::query_as(
        "SELECT status, manual, prevstatus, dndendtime, lastactivityat \
             FROM status WHERE userid = $1",
    )
    .bind(user_id)
    .fetch_optional(&pool)
    .await
    .expect("the status table is readable");
    row.map(|(status, manual, prev, dnd, last)| {
        (
            status.unwrap_or_default(),
            manual.unwrap_or(false),
            prev.unwrap_or_default(),
            dnd.unwrap_or(0),
            last.unwrap_or(0),
        )
    })
}

/// Set — or, with [`None`], delete — a `Systems` row.
///
/// `getOnboarding` synthesises `"false"` when the row is **absent**, and a real server already has
/// the row set to `"false"`, so the synthesised branch and the stored branch produce the same bytes
/// and two mutations of the decision survive. Planting a distinctive value is the only way for the
/// route to tell them apart. Go reads this through — there is no local cache layer over
/// `SystemStore.GetByName` — so both servers see the change immediately.
pub async fn set_system_value(name: &str, value: Option<&str>) -> bool {
    let Some(pool) = fixture_pool().await else {
        return false;
    };
    match value {
        Some(value) => {
            sqlx::query(
                "INSERT INTO systems (name, value) VALUES ($1, $2) \
                 ON CONFLICT (name) DO UPDATE SET value = EXCLUDED.value",
            )
            .bind(name)
            .bind(value)
            .execute(&pool)
            .await
            .expect("the systems row is written");
        }
        None => {
            sqlx::query("DELETE FROM systems WHERE name = $1")
                .bind(name)
                .execute(&pool)
                .await
                .expect("the systems row is removed");
        }
    }
    true
}

/// A unique-enough suffix for a planted row id.
///
/// Not `rand`: the suite is deterministic everywhere else, and a clock in microseconds is unique
/// enough for a row the test deletes moments later. Bounded well below the id column's width.
fn rand_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64 % 1_000_000_000)
        .unwrap_or(0)
}

/// Epoch **milliseconds**, the unit every timestamp column in this schema uses.
///
/// A planted row's `CreateAt` has to be a plausible instant: it is compared byte-for-byte against
/// Go's rendering of the same row, and it sorts against real rows. An earlier version added a
/// microsecond counter to a fixed base and produced timestamps in the year 2050.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(0))
        .unwrap_or(0)
}

/// Plant a `Schemes` row and return its id.
///
/// # No REST route can create one on this deployment
///
/// `POST /api/v4/schemes` is licence-gated and answers **501** on an unlicensed server — which is
/// exactly the behaviour the scheme suite also has to test. So the four scheme *reads* have no
/// fixture unless one is planted, and without a fixture `GET /api/v4/schemes` returns `[]` on both
/// servers and every assertion about its contents passes vacuously.
///
/// # The id is **unique per run**, and that is not tidiness
///
/// Go caches `SchemeStore.Get` by id in its local cache layer. Planting a fixed id and rewriting
/// the row — an `ON CONFLICT DO UPDATE` — leaves Go serving the *previous* run's `CreateAt` from
/// cache while this port reads the new row, and the parity comparison fails on a timestamp with
/// nothing wrong on either side. Measured. A fresh id has no cache entry, so the first read
/// populates it correctly.
///
/// `scope` is `"team"` or `"channel"`. The four default role names are left empty: they are
/// `varchar` columns Go fills with real role names when it creates a scheme, and nothing this
/// suite reads resolves them — a scheme is only ever *listed* here, never applied to a permission
/// check. The id carries the `mmrsscheme` prefix so [`purge_api_fixtures`] can find it.
pub async fn plant_scheme(scope: &str, tag: &str) -> Option<String> {
    let pool = fixture_pool().await?;
    let short = &tag[..tag.len().min(9)];
    let id = format!("mmrsscheme{short}{:0>7}", rand_suffix() % 10_000_000);
    let now = now_millis();
    sqlx::query(
        "INSERT INTO schemes (id, name, displayname, description, createat, updateat, deleteat, \
         scope, defaultteamadminrole, defaultteamuserrole, defaultchanneladminrole, \
         defaultchanneluserrole, defaultteamguestrole, defaultchannelguestrole, \
         defaultplaybookadminrole, defaultplaybookmemberrole, defaultrunadminrole, \
         defaultrunmemberrole) \
         VALUES ($1, $2, $3, 'planted by the parity suite', $4, $4, 0, $5, '', '', '', '', '', \
         '', '', '', '', '')",
    )
    .bind(&id)
    .bind(format!("mmrs-{short}-{}", rand_suffix() % 10_000_000))
    .bind(format!("mmrs {tag}"))
    .bind(now)
    .bind(scope)
    .execute(&pool)
    .await
    .expect("the scheme row is written");
    Some(id)
}

/// Point a team at a scheme, or with [`None`] detach it.
pub async fn set_team_scheme(team_id: &str, scheme_id: Option<&str>) -> bool {
    let Some(pool) = fixture_pool().await else {
        return false;
    };
    sqlx::query("UPDATE teams SET schemeid = $2 WHERE id = $1")
        .bind(team_id)
        .bind(scheme_id)
        .execute(&pool)
        .await
        .expect("the team's scheme is written");
    true
}

/// Point a channel at a scheme, or with [`None`] detach it.
pub async fn set_channel_scheme(channel_id: &str, scheme_id: Option<&str>) -> bool {
    let Some(pool) = fixture_pool().await else {
        return false;
    };
    sqlx::query("UPDATE channels SET schemeid = $2 WHERE id = $1")
        .bind(channel_id)
        .bind(scheme_id)
        .execute(&pool)
        .await
        .expect("the channel's scheme is written");
    true
}

/// Plant a custom role holding exactly `permissions`, and return its name.
///
/// # Why a fixture needs to invent a role
///
/// Three of the scheme routes are gated on three *different* sysconsole read permissions —
/// `..._permissions`, `..._teams`, `..._channels` — and **no stock role holds one without the
/// others**: `system_admin`, `system_manager`, `system_read_only_admin` and `system_user_manager`
/// all hold all three. So a mutation swapping one for another is invisible to any fixture built
/// from stock roles, and three of them survived the first run of `schemes.plan`.
///
/// A planted role also separates a sysconsole *read* from `manage_team`, which is what lets a
/// fixture observe `SanitizeTeams` doing anything: an admin can manage every team, so the
/// sanitizer is a no-op for the only session the suite had.
///
/// `permissions` is the space-separated form the column stores. `SchemeManaged` and `BuiltIn` are
/// false, matching a role an administrator created. The name carries the `mmrs_role_` prefix so
/// [`purge_api_fixtures`] can find it; Go's role cache is keyed by name and a fresh one has no
/// entry, so the first read populates it correctly — the same rule [`plant_scheme`] records.
pub async fn plant_role(tag: &str, permissions: &str) -> Option<String> {
    let pool = fixture_pool().await?;
    let name = format!("mmrs_role_{tag}");
    let id = format!("mmrsrole{tag:0>18}");
    let now = now_millis();
    sqlx::query(
        "INSERT INTO roles (id, name, displayname, description, createat, updateat, deleteat, \
         permissions, schememanaged, builtin, schemeid) \
         VALUES ($1, $2, $2, 'planted by the parity suite', $3, $3, 0, $4, false, false, NULL) \
         ON CONFLICT (name) DO UPDATE SET permissions = EXCLUDED.permissions",
    )
    .bind(&id)
    .bind(&name)
    .bind(now)
    .bind(permissions)
    .execute(&pool)
    .await
    .expect("the role row is written");
    Some(name)
}

/// Plant a channel row of an arbitrary type, returning its id.
///
/// `POST /api/v4/channels` accepts only `O` and `P`, so a **board** (`BO`) has to be written
/// directly. `GetChannelsByScheme` excludes only `S`, while `ChannelStore::Get` excludes
/// everything but `('O','P','D','G')` — without a board in the fixture those two filters return
/// the same rows and a mutation swapping them survives.
pub async fn plant_channel_of_type(team_id: &str, channel_type: &str, tag: &str) -> Option<String> {
    let pool = fixture_pool().await?;
    let id = format!("mmrschan{:0>18}", rand_suffix());
    let now = now_millis();
    sqlx::query(
        "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname, \
         name, header, purpose, lastpostat, totalmsgcount, extraupdateat, creatorid, schemeid, \
         groupconstrained, shared, totalmsgcountroot, lastrootpostat, defaultcategoryname, \
         discoverable, autotranslation) \
         VALUES ($1, $2, $2, 0, $3, $4::channel_type, $5, $6, '', '', 0, 0, 0, '', NULL, NULL, \
         NULL, 0, 0, '', false, false)",
    )
    .bind(&id)
    .bind(now)
    .bind(team_id)
    .bind(channel_type)
    .bind(format!("mmrs board {tag}"))
    .bind(format!("mmrs-parity-{tag}"))
    .execute(&pool)
    .await
    .expect("the channel row is written");
    Some(id)
}

/// Set a team's display name through Go, so both servers see the change and its caches are
/// updated the way any client would update them.
///
/// The scheme suite needs teams whose display-name order is the **reverse** of their name order,
/// to tell `ORDER BY DisplayName` from `ORDER BY Name`. `create_team` derives both from one tag,
/// so they always agree until something changes one of them.
pub async fn set_team_display_name(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    display_name: &str,
) {
    let response = client
        .put(format!("{GO}/api/v4/teams/{team_id}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "id": team_id, "display_name": display_name }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "renaming the fixture team failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// A second `mm-api`, started on its own port with extra environment, for the life of the guard.
///
/// # Why the suite needs one
///
/// Several routes are gated on a setting that lives **only** in the environment — Go strips
/// `FeatureFlags` before persisting the configuration, so the flag behind the fifteen `/recaps`
/// routes has no database representation at all. The suite can therefore reach only one side of
/// those gates against the shared server on :8066, and a mutation that ignores the gate entirely
/// is invisible: "always refuse" and "refuse unless enabled" are the same program when the
/// feature is off. Measured — `recaps-gate-ignored` survived the first run of `recaps.plan`.
///
/// This starts the **same binary** `scripts/parity.sh` just built, with the same database and the
/// same upstream, differing only in the variables under test. Killed on drop, including on a
/// panic, so a failing test cannot leave a stray server bound to the port.
pub struct SecondServer {
    child: std::process::Child,
    pub base: String,
}

impl SecondServer {
    /// Start one on `port` with `env` overlaid, and wait for it to answer.
    ///
    /// Returns [`None`] when the binary is not where `parity.sh` leaves it — a `cargo test` run
    /// outside the harness — so a caller can skip rather than fail for the wrong reason.
    pub async fn start(port: u16, env: &[(&str, &str)]) -> Option<Self> {
        let binary =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mm-api");
        if !binary.exists() {
            return None;
        }
        let database_url = std::env::var("DATABASE_URL").ok()?;

        let mut command = std::process::Command::new(binary);
        command
            .env("DATABASE_URL", database_url)
            .env("MM_API_LISTEN", format!("127.0.0.1:{port}"))
            .env("MM_GO_UPSTREAM", GO)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        for (key, value) in env {
            command.env(key, value);
        }
        let child = command.spawn().ok()?;

        let base = format!("http://127.0.0.1:{port}");
        let client = client();
        for _ in 0..60 {
            if client
                .get(format!("{base}/api/v4/system/ping"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return Some(Self { child, base });
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        // Never came up: kill it rather than leaving it, and let the caller skip.
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        None
    }
}

impl Drop for SecondServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------------------------
// Websocket client
//
// A write route's answer is only half of what it does: Go also publishes an event, and a port
// that persists the right row while broadcasting nothing is wrong in a way no HTTP comparison can
// see. So the suite has to be a websocket client on both servers at once.
// ---------------------------------------------------------------------------------------------

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// A live websocket connection to one of the two servers, with everything it has been sent.
pub struct SocketProbe {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    /// Frames exactly as they arrived, including the trailing newline Go's `json.Encoder` leaves
    /// on everything that did not take the precompute path. Kept raw, because that newline and
    /// the spacing after each colon are the two things a value comparison cannot see.
    pub raw: Vec<String>,
}

impl SocketProbe {
    /// Connect and read the `hello` frame, leaving the probe ready to collect what follows.
    ///
    /// The token goes in the `Authorization` header, never the query string: `handlers.go:281`
    /// rejects a non-OAuth session presented as `?access_token=` with a 401, so a probe that used
    /// the query string would be testing that rejection instead of the socket.
    pub async fn connect(base: &str, token: &str) -> SocketProbe {
        let url = format!("{}/api/v4/websocket", base.replace("http://", "ws://"));
        let request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                url.as_str(),
            )
            .expect("a websocket request");
        let mut request = request;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {token}").parse().expect("a header value"),
        );
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .unwrap_or_else(|e| panic!("{base} websocket: {e}"));
        let mut probe = SocketProbe {
            socket,
            raw: Vec::new(),
        };
        probe.collect_for(Duration::from_millis(400)).await;
        assert_eq!(
            probe.events_named("hello").len(),
            1,
            "{base} did not send exactly one hello: {:?}",
            probe.raw
        );
        probe.raw.clear();
        probe
    }

    /// Send one `WebSocketRequest`.
    pub async fn send(&mut self, request: serde_json::Value) {
        self.socket
            .send(Message::Text(request.to_string().into()))
            .await
            .expect("the socket accepts a frame");
    }

    /// Read whatever arrives within `window`, appending to [`SocketProbe::raw`].
    ///
    /// A fixed window rather than "wait for N frames" on purpose: the assertion a write route
    /// needs is usually *how many* events it published, and a reader that stops at the expected
    /// count cannot tell one from two.
    pub async fn collect_for(&mut self, window: Duration) {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            match tokio::time::timeout(remaining, self.socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => self.raw.push(text.to_string()),
                // Pings are answered by the library; nothing else is expected.
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(err))) => panic!("websocket error: {err}"),
                Ok(None) => return,
                Err(_) => return,
            }
        }
    }

    /// Collect until `found` says the frames the caller is waiting for have arrived, or `window`
    /// expires. Returns whether they arrived.
    ///
    /// **Prefer this to [`SocketProbe::collect_for`] for anything that asserts a count.** A fixed
    /// window encodes an assumption about how fast the server is, and that assumption changed:
    /// the Go server used to run under qemu and now runs native, roughly six times quicker, which
    /// turned three passing socket assertions into intermittent failures on the same code. Waiting
    /// for the event rather than for the clock is the only version of the test that means the same
    /// thing on both. `collect_for` remains correct for the opposite assertion — that nothing
    /// *else* arrives — where the whole point is to wait out a window.
    ///
    /// The predicate sees every frame collected so far, parsed, including any that arrived before
    /// this call.
    pub async fn collect_until<F>(&mut self, window: Duration, found: F) -> bool
    where
        F: Fn(&[serde_json::Value]) -> bool,
    {
        if found(&self.frames()) {
            return true;
        }
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match tokio::time::timeout(remaining, self.socket.next()).await {
                Ok(Some(Ok(Message::Text(text)))) => {
                    self.raw.push(text.to_string());
                    if found(&self.frames()) {
                        return true;
                    }
                }
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(err))) => panic!("websocket error: {err}"),
                Ok(None) => return false,
                Err(_) => return false,
            }
        }
    }

    /// The collected frames as JSON values.
    pub fn frames(&self) -> Vec<serde_json::Value> {
        self.raw
            .iter()
            .map(|raw| serde_json::from_str(raw).expect("a frame decodes"))
            .collect()
    }

    /// Every collected frame whose `event` is `name`.
    pub fn events_named(&self, name: &str) -> Vec<serde_json::Value> {
        self.frames()
            .into_iter()
            .filter(|frame| frame.get("event").and_then(|e| e.as_str()) == Some(name))
            .collect()
    }

    /// The frames that answer a request this probe sent, raw and parsed, in arrival order.
    ///
    /// **A socket is not isolated the way a request is.** Anything else running against the same
    /// server broadcasts to this connection too — a whole-suite run put five `new_user`,
    /// `posted` and `user_added` frames on the admin's socket between one request and its answer.
    /// A test that counted frames was really counting the rest of the suite. Responses are
    /// separable because only they carry `seq_reply`.
    pub fn responses(&self) -> Vec<(&str, serde_json::Value)> {
        self.raw
            .iter()
            .map(|raw| {
                (
                    raw.as_str(),
                    serde_json::from_str::<serde_json::Value>(raw).expect("a frame decodes"),
                )
            })
            .filter(|(_, frame)| frame.get("seq_reply").is_some() || frame.get("status").is_some())
            .collect()
    }
}
