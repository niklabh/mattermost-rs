//! Cross-server parity for account conversion and the guest pair: `convertUserToBot`,
//! `convertBotToUser`, `promoteGuestToUser` and `demoteUserToGuest`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity user_convert
//! ```
//!
//! # These four routes turn accounts into other kinds of account, so nothing is compared in place
//!
//! Both servers share one database, so an account converted by Go cannot then be converted by
//! this one. Every success test builds **two** fixtures — one driven per server — and compares
//! the answers with the id, the username and the clocks normalised to presence assertions.
//! [D-190]'s rule, and here it is load-bearing rather than hygiene: a bot is keyed on the user id.
//!
//! # The gate orders are the subject, and they are all different
//!
//! Each of the four checks its preconditions in a different sequence, and each sequence is
//! measurable only by sending **two** wrong things at once — a caller with no permission asking
//! about an id that does not exist. Measured against Go on this stack:
//!
//! | route | that request answers |
//! |---|---|
//! | `convert_to_bot` | **404** — the user is fetched first |
//! | `promote` | **403** — the permission is checked first |
//! | `demote` | **501** — the *licence* is checked before either |
//! | `convert_to_user` (bad body) | **400** — bot, then body, then permission |
//!
//! A port that tidied any of these into a uniform order passes every single-fault test.
//!
//! # Guest accounts are unreachable through the API on this deployment
//!
//! `GuestAccountsSettings.Enable` needs a licence, so no route here can *create* a guest. The
//! promotion tests plant one — `Users.Roles`, `TeamMembers` and `ChannelMembers` written directly
//! — which is the same treatment `plant_bot` gives a bot and for the same reason. Go caches
//! users, so each planting is followed by a cache invalidation before Go is asked anything.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, client, create_plain_user, create_team, delete_plain_user,
    go_minted_token, stack_enabled,
};

/// An id that is well-formed and names nothing.
const MISSING: &str = "abcdefghijklmnopqrstuvwxyz";

/// Send one request and read `(status, raw body)`, asserting a Rust answer was served here.
async fn send(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: Option<&[u8]>,
) -> (u16, Vec<u8>) {
    let mut request = http
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request
            .header("Content-Type", "application/json")
            .body(body.to_vec());
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.bytes().await.expect("a body").to_vec())
}

/// The same refused request to both servers.
async fn both(
    http: &reqwest::Client,
    token: &str,
    path: &str,
    body: Option<&[u8]>,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let go = send(http, GO, token, path, body).await;
    let rs = send(http, RUST, token, path, body).await;
    (go, rs)
}

/// Assert both servers refused identically and return Go's parsed body.
async fn same_refusal(
    http: &reqwest::Client,
    token: &str,
    path: &str,
    body: Option<&[u8]>,
    expected_status: u16,
    expected_id: &str,
) -> serde_json::Value {
    let ((go_status, go), (rs_status, rs)) = both(http, token, path, body).await;
    assert_eq!(rs_status, go_status, "status for {path}");
    assert_eq!(go_status, expected_status, "Go's status for {path}");
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    assert_eq!(parsed["id"], expected_id, "error id for {path}");
    parsed
}

/// Everything about a `model.Bot` two independently converted accounts can be expected to share.
fn normalise_bot(raw: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value =
        serde_json::from_slice(raw).unwrap_or_else(|e| panic!("not a bot: {e}: {raw:?}"));
    let object = value.as_object_mut().expect("an object");
    for key in ["user_id", "owner_id", "username", "display_name"] {
        let present = object
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        object.insert(key.to_owned(), serde_json::json!(present));
    }
    for key in ["create_at", "update_at"] {
        let nonzero = object.get(key).and_then(|v| v.as_i64()).unwrap_or(0) > 0;
        object.insert(key.to_owned(), serde_json::json!(nonzero));
    }
    value
}

/// The same for a `model.User`, which is what the reverse conversion answers with.
fn normalise_user(raw: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value =
        serde_json::from_slice(raw).unwrap_or_else(|e| panic!("not a user: {e}: {raw:?}"));
    let object = value.as_object_mut().expect("an object");
    for key in ["id", "username", "email"] {
        let present = object
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        object.insert(key.to_owned(), serde_json::json!(present));
    }
    for key in ["create_at", "update_at", "last_password_update"] {
        let nonzero = object.get(key).and_then(|v| v.as_i64()).unwrap_or(0) > 0;
        object.insert(key.to_owned(), serde_json::json!(nonzero));
    }
    value
}

/// `(roles, authservice, deleteat)` straight off the `Users` row, and whether a `Bots` row exists.
async fn account_row(user_id: &str) -> Option<(String, String, i64, bool)> {
    let pool = common::fixture_pool().await?;
    let row: (String, Option<String>, i64, Option<i64>) = sqlx::query_as(
        "SELECT u.roles, u.authservice, u.deleteat,
                (SELECT count(*) FROM bots b WHERE b.userid = u.id)
           FROM users u WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the account row is readable");
    Some((
        row.0,
        row.1.unwrap_or_default(),
        row.2,
        row.3.unwrap_or(0) > 0,
    ))
}

/// `Users.nickname` and `Users.position` — the two fields the patch tests write.
async fn profile_of(user_id: &str) -> Option<(String, String)> {
    let pool = common::fixture_pool().await?;
    let row: (String, String) =
        sqlx::query_as("SELECT nickname, position FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .expect("the profile is readable");
    Some(row)
}

/// How many live sessions this account has.
async fn session_count(user_id: &str) -> Option<i64> {
    let pool = common::fixture_pool().await?;
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM sessions WHERE userid = $1")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("sessions are countable");
    Some(count)
}

/// Turn an ordinary account into a guest, in the three places the role lives.
///
/// `PromoteGuestToUser` writes all three back, so a fixture that set only `Users.Roles` would
/// leave the two membership flags already at their post-promotion values and the test could not
/// tell a working port from one that skipped both `UPDATE`s.
async fn make_guest(user_id: &str) -> bool {
    let Some(pool) = common::fixture_pool().await else {
        return false;
    };
    for statement in [
        "UPDATE users SET roles = 'system_guest' WHERE id = $1",
        "UPDATE teammembers SET schemeuser = false, schemeguest = true WHERE userid = $1",
        "UPDATE channelmembers SET schemeuser = false, schemeguest = true WHERE userid = $1",
    ] {
        sqlx::query(statement)
            .bind(user_id)
            .execute(&pool)
            .await
            .expect("the guest fixture is written");
    }
    true
}

/// `(roles, guest team rows, guest channel rows, user channel rows, **admin** channel rows)`.
///
/// The last column has nothing to do with promotion and everything to do with
/// `JoinDefaultChannels`' `shouldBeAdmin` argument, which is `false` and whose other value would
/// otherwise be invisible: a promoted guest rejoining `off-topic` as a channel administrator
/// leaves the roles, both scheme flags and the response body exactly as they are.
async fn guest_shape(user_id: &str) -> Option<(String, i64, i64, i64, i64)> {
    let pool = common::fixture_pool().await?;
    let row: (String, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT u.roles,
                (SELECT count(*) FROM teammembers t WHERE t.userid = u.id AND t.schemeguest),
                (SELECT count(*) FROM channelmembers c WHERE c.userid = u.id AND c.schemeguest),
                (SELECT count(*) FROM channelmembers c WHERE c.userid = u.id AND c.schemeuser),
                (SELECT count(*) FROM channelmembers c WHERE c.userid = u.id AND c.schemeadmin)
           FROM users u WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("the guest shape is readable");
    Some(row)
}

// ---------------------------------------------------------------------------------------------
// convertUserToBot
// ---------------------------------------------------------------------------------------------

/// **The user is fetched before the permission is checked**, so a caller with neither gets a 404.
///
/// That is the opposite of `promoteGuestToUser` fifty lines away in the same Go file, and the
/// pair is the reason this test and its promote twin both send a plain user at a missing id.
#[tokio::test]
async fn convert_to_bot_gates_fire_in_gos_order() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "cvtbgate").await;
    let user = create_plain_user(&http, &admin, &team, "cvtbgate").await;

    // An id-shaped segment that is not an id never reaches a handler's body.
    same_refusal(
        &http,
        &admin,
        "/api/v4/users/notanid/convert_to_bot",
        None,
        400,
        "api.context.invalid_url_param.app_error",
    )
    .await;

    // Two faults at once: no `manage_system` **and** no such user. The fetch wins.
    same_refusal(
        &http,
        &user.token,
        &format!("/api/v4/users/{MISSING}/convert_to_bot"),
        None,
        404,
        "app.user.missing_account.const",
    )
    .await;

    // One fault: the user exists, so the permission is what refuses.
    same_refusal(
        &http,
        &user.token,
        &format!("/api/v4/users/{}/convert_to_bot", user.id),
        None,
        403,
        "api.context.permissions.app_error",
    )
    .await;

    // And the admin gets the 404 too, which is what makes the 404 above a *fetch* and not a
    // disguised permission answer.
    same_refusal(
        &http,
        &admin,
        &format!("/api/v4/users/{MISSING}/convert_to_bot"),
        None,
        404,
        "app.user.missing_account.const",
    )
    .await;

    assert_eq!(
        account_row(&user.id).await.map(|row| row.3),
        Some(false),
        "four refusals must not have made a bot",
    );

    delete_plain_user(&http, &admin, &user.id).await;
}

/// The conversion itself: one `Bots` row, every session gone, and the `Users` row untouched.
///
/// # Why the session count is the assertion and not the `Bots` row alone
///
/// `RevokeAllSessions` runs **after** the insert, so a port that dropped it would write the same
/// row, answer the same body and leave a logged-in bot — an account that can still spend its
/// token. Nothing on the wire shows it.
#[tokio::test]
async fn converting_an_account_writes_a_bot_and_revokes_its_sessions() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let _users = common::USER_COUNT.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "cvtbrun").await;

    let go_user = create_plain_user(&http, &admin, &team, "cvtbgo").await;
    let rs_user = create_plain_user(&http, &admin, &team, "cvtbrs").await;

    // `create_plain_user` logs in, so each account starts with at least one session.
    for user in [&go_user, &rs_user] {
        assert!(
            session_count(&user.id).await.unwrap_or(0) > 0,
            "the fixture account is logged in before the conversion",
        );
    }

    let (go_status, go_body) = send(
        &http,
        GO,
        &admin,
        &format!("/api/v4/users/{}/convert_to_bot", go_user.id),
        None,
    )
    .await;
    let (rs_status, rs_body) = send(
        &http,
        RUST,
        &admin,
        &format!("/api/v4/users/{}/convert_to_bot", rs_user.id),
        None,
    )
    .await;

    assert_eq!(
        go_status,
        200,
        "Go converted: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        200,
        "we converted: {}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(
        normalise_bot(&rs_body),
        normalise_bot(&go_body),
        "the converted bot differs",
    );

    // `w.Write(js)` and not `json.NewEncoder`: **no** trailing newline, unlike its sibling route.
    assert!(
        !go_body.ends_with(b"\n"),
        "Go's convert_to_bot body carries no trailing newline",
    );
    assert!(
        !rs_body.ends_with(b"\n"),
        "and neither does ours: {:?}",
        String::from_utf8_lossy(&rs_body),
    );

    // `display_name` is `GetDisplayName(ShowUsername)` — the username, not the first name, and
    // not what a later `GET /bots/{id}` answers. Pinned on Go's body, which is the oracle.
    let go_bot: serde_json::Value = serde_json::from_slice(&go_body).expect("a bot");
    assert_eq!(
        go_bot["display_name"], go_bot["username"],
        "BotFromUser takes the username for the display name",
    );
    assert_eq!(
        go_bot["owner_id"], go_bot["user_id"],
        "a converted account owns itself",
    );
    assert!(
        go_bot.get("description").is_none(),
        "an empty description is omitted",
    );

    for user in [&go_user, &rs_user] {
        let (roles, auth_service, delete_at, is_bot) =
            account_row(&user.id).await.expect("the account row");
        assert!(is_bot, "{} should now have a Bots row", user.id);
        assert_eq!(roles, "system_user", "the conversion changes no roles");
        assert_eq!(auth_service, "", "and writes no auth service");
        assert_eq!(delete_at, 0, "and does not deactivate the account");
        assert_eq!(
            session_count(&user.id).await,
            Some(0),
            "every session of {} is revoked",
            user.id,
        );
    }

    common::unplant_bot(&go_user.id).await;
    common::unplant_bot(&rs_user.id).await;
}

/// Converting an account that is already a bot is a **500**, from the primary key.
///
/// Not a 400 and not a 409: `Bot().Save`'s unique violation is not an `*model.AppError`, so
/// `errors.As` misses it and the default arm answers `app.bot.createbot.internal_error` with a
/// `where` of `CreateBot`. Measured; a port that pre-checked would answer something tidier.
#[tokio::test]
async fn converting_a_bot_again_is_the_save_conflict() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    let Some(bot) = common::plant_bot("cvtbtwice", me, 0).await else {
        return; // no DATABASE_URL
    };

    same_refusal(
        &http,
        &admin,
        &format!("/api/v4/users/{bot}/convert_to_bot"),
        None,
        500,
        "app.bot.createbot.internal_error",
    )
    .await;

    common::unplant_bot(&bot).await;
}

/// An account carrying an `AuthService` is **handed to Go**, and the handover happens before the
/// `Bots` insert.
///
/// # The proof is Go's 200, not the missing header
///
/// `App.ConvertUserToBot`'s first act on such an account is `UpdateUserAuth`; neither it nor
/// `UserStore.UpdateAuthData` is ported ([D-510]). The absent `x-mmrs-served-by` shows the request
/// left, but it does not on its own show that nothing was written first. Go's **200** does: had
/// this server inserted the `Bots` row and then forwarded, Go's own `Save` would have hit the same
/// primary key the test above pins and answered 500.
#[tokio::test]
async fn an_account_with_an_auth_service_is_handed_to_go() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let _users = common::USER_COUNT.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "cvtbauth").await;
    let user = create_plain_user(&http, &admin, &team, "cvtbauth").await;

    if !common::set_user_auth_service(&user.id, "gitlab").await {
        return; // no DATABASE_URL
    }
    common::invalidate_go_caches(&http, &admin).await;

    let path = format!("/api/v4/users/{}/convert_to_bot", user.id);
    let response = http
        .post(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("mm-api answers");
    let status = response.status().as_u16();
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "a federated account must be forwarded, not converted here",
    );
    let body = response.bytes().await.expect("a body").to_vec();
    assert_eq!(
        status,
        200,
        "Go's own conversion must succeed, which proves nothing was written before the \
         forward: {}",
        String::from_utf8_lossy(&body),
    );

    let (_, auth_service, _, is_bot) = account_row(&user.id).await.expect("the account row");
    assert!(is_bot, "Go converted it");
    assert_eq!(
        auth_service, "",
        "and cleared the auth service on the way, which is the step this server lacks",
    );

    common::unplant_bot(&user.id).await;
}

// ---------------------------------------------------------------------------------------------
// convertBotToUser
// ---------------------------------------------------------------------------------------------

/// **Bot, then body, then permission** — and each step is pinned by a request that fails two.
#[tokio::test]
async fn convert_to_user_gates_fire_in_gos_order() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "cvtugate").await;
    let user = create_plain_user(&http, &admin, &team, "cvtugate").await;
    let me = common::logged_in_user_id();

    let Some(bot) = common::plant_bot("cvtugate", me, 0).await else {
        return; // no DATABASE_URL
    };

    // `RequireBotUserId` does **not** resolve `me`, unlike every `{user_id}` route.
    for id in ["notanid", "me"] {
        same_refusal(
            &http,
            &admin,
            &format!("/api/v4/bots/{id}/convert_to_user"),
            Some(br#"{"password":"Mmrs-Plain-1234"}"#),
            400,
            "api.context.invalid_url_param.app_error",
        )
        .await;
    }

    // No bot, no permission, and a body that will not decode: the **bot** refuses first.
    same_refusal(
        &http,
        &user.token,
        &format!("/api/v4/bots/{MISSING}/convert_to_user"),
        Some(b"not json"),
        404,
        "store.sql_bot.get.missing.app_error",
    )
    .await;

    // A real bot, no permission, and four bodies that all reach the same 400 — so the body is
    // read before the permission, and the four are indistinguishable from each other.
    for body in [
        &b""[..],
        &b"not json"[..],
        &b"{}"[..],
        &br#"{"password":""}"#[..],
        &b"null"[..],
    ] {
        same_refusal(
            &http,
            &user.token,
            &format!("/api/v4/bots/{bot}/convert_to_user"),
            Some(body),
            400,
            "api.context.invalid_body_param.app_error",
        )
        .await;
    }

    // Only a body that names a non-empty password reaches the permission gate.
    same_refusal(
        &http,
        &user.token,
        &format!("/api/v4/bots/{bot}/convert_to_user"),
        Some(br#"{"password":"Mmrs-Plain-1234"}"#),
        403,
        "api.context.permissions.app_error",
    )
    .await;

    assert_eq!(
        account_row(&bot).await.map(|row| row.3),
        Some(true),
        "eleven refusals must leave the bot a bot",
    );

    common::unplant_bot(&bot).await;
    delete_plain_user(&http, &admin, &user.id).await;
}

/// A bot becomes a user again: the patch applies, the password works, the `Bots` row goes.
///
/// # `is_bot` is still `true` in the answer, and `last_password_update` is stale
///
/// Both measured. The struct was read from the `Bots` join before the delete, and
/// `UpdatePassword` writes the column without touching the copy being returned. A port that
/// re-read the row to answer would be more truthful and would not match.
#[tokio::test]
async fn a_bot_converts_back_into_a_user() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    let Some(go_bot) = common::plant_bot("cvtubackg", me, 0).await else {
        return; // no DATABASE_URL
    };
    let rs_bot = common::plant_bot("cvtubackr", me, 0)
        .await
        .expect("a second bot");
    // `plant_bot` derives the description and the first name from the tag, and the two tags have
    // to differ because the ids do. Both reach the **user** body — `bot_description` and
    // `first_name` are `model.User` fields — so a byte comparison without this fails on the
    // fixture rather than on the route.
    for bot in [&go_bot, &rs_bot] {
        common::set_bot_fixture_text(bot, "a bot the convert suite planted", "Convert").await;
    }
    common::invalidate_go_caches(&http, &admin).await;

    let body = br#"{"password":"Mmrs-Convert-1234","nickname":"converted","position":"desk"}"#;
    let (go_status, go_raw) = send(
        &http,
        GO,
        &admin,
        &format!("/api/v4/bots/{go_bot}/convert_to_user"),
        Some(body),
    )
    .await;
    let (rs_status, rs_raw) = send(
        &http,
        RUST,
        &admin,
        &format!("/api/v4/bots/{rs_bot}/convert_to_user"),
        Some(body),
    )
    .await;

    assert_eq!(go_status, 200, "Go: {}", String::from_utf8_lossy(&go_raw));
    assert_eq!(rs_status, 200, "us: {}", String::from_utf8_lossy(&rs_raw));
    assert_eq!(
        normalise_user(&rs_raw),
        normalise_user(&go_raw),
        "the converted user differs",
    );

    // `json.NewEncoder(w).Encode` — exactly one trailing newline, where its sibling route has none.
    assert!(go_raw.ends_with(b"\n"), "Go's body ends in a newline");
    assert!(rs_raw.ends_with(b"\n"), "and so does ours");

    let go_user: serde_json::Value = serde_json::from_slice(&go_raw).expect("a user");
    assert_eq!(
        go_user["is_bot"],
        serde_json::json!(true),
        "the answer still says is_bot, because the delete comes after the read",
    );
    assert_eq!(go_user["nickname"], "converted", "the patch applied");
    assert_eq!(go_user["position"], "desk");
    assert_eq!(
        go_user["roles"], "system_user",
        "no set_system_admin, so no role change",
    );

    for bot in [&go_bot, &rs_bot] {
        let (roles, _, _, is_bot) = account_row(bot).await.expect("the account row");
        assert!(!is_bot, "{bot} is no longer a bot");
        assert_eq!(roles, "system_user");
        assert_eq!(
            profile_of(bot).await,
            Some(("converted".to_owned(), "desk".to_owned())),
            "the patch reached the row for {bot}",
        );
    }

    // The planted bot's `Users` row carries an empty password hash, so a successful login is
    // proof the password write happened and not merely that the row survived.
    for bot in [&go_bot, &rs_bot] {
        let username = common::username_of(&http, &admin, bot).await;
        let response = http
            .post(format!("{GO}/api/v4/users/login"))
            .json(&serde_json::json!({
                "login_id": username,
                "password": "Mmrs-Convert-1234",
            }))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(
            response.status().as_u16(),
            200,
            "the converted account can log in with its new password: {}",
            response.text().await.unwrap_or_default(),
        );
    }

    common::unplant_bot(&go_bot).await;
    common::unplant_bot(&rs_bot).await;
}

/// `?set_system_admin=true` **appends** `system_admin`, and only when it is not already there.
#[tokio::test]
async fn set_system_admin_appends_the_role_exactly_once() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    let Some(go_bot) = common::plant_bot("cvtusag", me, 0).await else {
        return; // no DATABASE_URL
    };
    let rs_bot = common::plant_bot("cvtusar", me, 0).await.expect("a bot");
    // The second pair already holds the role, which is the branch `!user.IsInRole(...)` guards.
    let go_already = common::plant_bot("cvtusahg", me, 0).await.expect("a bot");
    let rs_already = common::plant_bot("cvtusahr", me, 0).await.expect("a bot");
    for bot in [&go_already, &rs_already] {
        common::set_user_roles(bot, "system_user system_admin").await;
    }
    common::invalidate_go_caches(&http, &admin).await;

    let body = br#"{"password":"Mmrs-Convert-1234"}"#;
    for (base, bot) in [
        (GO, &go_bot),
        (RUST, &rs_bot),
        (GO, &go_already),
        (RUST, &rs_already),
    ] {
        let (status, raw) = send(
            &http,
            base,
            &admin,
            &format!("/api/v4/bots/{bot}/convert_to_user?set_system_admin=true"),
            Some(body),
        )
        .await;
        assert_eq!(
            status,
            200,
            "{base} {bot}: {}",
            String::from_utf8_lossy(&raw)
        );
    }

    for bot in [&go_bot, &rs_bot] {
        let (roles, ..) = account_row(bot).await.expect("the row");
        assert_eq!(
            roles, "system_user system_admin",
            "the role is appended to the existing list, in that order",
        );
    }
    for bot in [&go_already, &rs_already] {
        let (roles, ..) = account_row(bot).await.expect("the row");
        assert_eq!(
            roles, "system_user system_admin",
            "an account that already holds it gains no duplicate",
        );
    }

    // Leave no new administrators behind.
    for bot in [&go_bot, &rs_bot, &go_already, &rs_already] {
        common::unplant_bot(bot).await;
    }
}

/// **A password too short is refused *after* the patch has been written.**
///
/// The password is only checked for *emptiness* at the api layer; its validity is checked inside
/// `UpdatePassword`, which runs after `UpdateUser`. So this 400 leaves the account patched and
/// still a bot — a partial write with no transaction around it, reproduced rather than fixed.
/// A port that validated the password up front would answer the same 400 and write nothing, and
/// this is the only test that can tell the two apart.
#[tokio::test]
async fn a_short_password_is_refused_after_the_patch_is_already_written() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    let Some(go_bot) = common::plant_bot("cvtushortg", me, 0).await else {
        return; // no DATABASE_URL
    };
    let rs_bot = common::plant_bot("cvtushortr", me, 0).await.expect("a bot");

    let body = br#"{"password":"a","nickname":"half-written"}"#;
    let ((go_status, go), (rs_status, rs)) = (
        send(
            &http,
            GO,
            &admin,
            &format!("/api/v4/bots/{go_bot}/convert_to_user"),
            Some(body),
        )
        .await,
        send(
            &http,
            RUST,
            &admin,
            &format!("/api/v4/bots/{rs_bot}/convert_to_user"),
            Some(body),
        )
        .await,
    );
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 400);
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, "convert_to_user");
    assert_eq!(parsed["id"], "model.user.is_valid.pwd_min_length.app_error");

    for bot in [&go_bot, &rs_bot] {
        let (_, _, _, is_bot) = account_row(bot).await.expect("the row");
        assert!(is_bot, "{bot} is still a bot — the delete never ran");
        assert_eq!(
            profile_of(bot).await.map(|p| p.0),
            Some("half-written".to_owned()),
            "and the patch was written before the password failed",
        );
    }

    common::unplant_bot(&go_bot).await;
    common::unplant_bot(&rs_bot).await;
}

// ---------------------------------------------------------------------------------------------
// promoteGuestToUser
// ---------------------------------------------------------------------------------------------

/// **The permission is checked before the user is fetched** — the mirror image of
/// `convert_to_bot`'s order, with the same two-fault request producing 403 instead of 404.
#[tokio::test]
async fn promote_gates_fire_in_gos_order() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "promgate").await;
    let user = create_plain_user(&http, &admin, &team, "promgate").await;

    same_refusal(
        &http,
        &admin,
        "/api/v4/users/notanid/promote",
        None,
        400,
        "api.context.invalid_url_param.app_error",
    )
    .await;

    // No `promote_guest` **and** no such user: the permission wins, so the caller learns nothing
    // about whether the id exists.
    same_refusal(
        &http,
        &user.token,
        &format!("/api/v4/users/{MISSING}/promote"),
        None,
        403,
        "api.context.permissions.app_error",
    )
    .await;

    // The admin holds it and gets the fetch's 404 — no licence gate anywhere on this route, which
    // is what separates it from its twin.
    same_refusal(
        &http,
        &admin,
        &format!("/api/v4/users/{MISSING}/promote"),
        None,
        404,
        "app.user.missing_account.const",
    )
    .await;

    // An account that is not a guest is **501**, not 400.
    same_refusal(
        &http,
        &admin,
        &format!("/api/v4/users/{}/promote", user.id),
        None,
        501,
        "api.user.promote_guest_to_user.no_guest.app_error",
    )
    .await;

    delete_plain_user(&http, &admin, &user.id).await;
}

/// A guest signing in by magic link is refused with a **different** 501, after the guest check.
#[tokio::test]
async fn a_magic_link_guest_is_refused_by_its_own_501() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "prommagic").await;
    let user = create_plain_user(&http, &admin, &team, "prommagic").await;

    if !make_guest(&user.id).await {
        return; // no DATABASE_URL
    }
    // `IsMagicLinkEnabled` is `AuthService == "magic_link" && IsGuest()` — both halves, so an
    // ordinary account with the same auth service reaches the `no_guest` refusal instead.
    common::set_user_auth_service(&user.id, "magic_link").await;
    common::invalidate_go_caches(&http, &admin).await;

    same_refusal(
        &http,
        &admin,
        &format!("/api/v4/users/{}/promote", user.id),
        None,
        501,
        "api.user.promote_guest_to_user.magic_link_enabled.app_error",
    )
    .await;

    assert_eq!(
        guest_shape(&user.id).await.map(|shape| shape.0),
        Some("system_guest".to_owned()),
        "the refusal wrote nothing",
    );

    common::set_user_auth_service(&user.id, "").await;
    delete_plain_user(&http, &admin, &user.id).await;
}

/// The promotion itself: `Users.Roles`, `TeamMembers` and `ChannelMembers` all move, and the
/// account is re-added to the default channels it had been removed from.
///
/// # Three writes in one transaction, and only one of them is visible on the wire
///
/// The response is `{"status":"OK"}` either way. What separates a working port from one that
/// wrote the roles and skipped the two membership updates is the rows, so the rows are what this
/// reads — before and after, per server.
#[tokio::test]
async fn a_guest_is_promoted_and_rejoins_the_default_channels() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _users = common::USER_COUNT.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "promrun").await;

    let go_user = create_plain_user(&http, &admin, &team, "promgo").await;
    let rs_user = create_plain_user(&http, &admin, &team, "promrs").await;

    // Take each account out of `off-topic` so `JoinDefaultChannels` has something to do. It is
    // the only part of `App.PromoteGuestToUser` that writes outside the store transaction.
    //
    // **`town-square` cannot be used**: Go refuses to remove anybody from the default channel
    // (`api.channel.remove.default.app_error`, measured as a 400), so the only default channel a
    // fixture can empty and watch refill is the other one.
    let off_topic: String = {
        let channel: serde_json::Value = http
            .get(format!("{GO}/api/v4/teams/{team}/channels/name/off-topic"))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await
            .expect("Go answers")
            .json()
            .await
            .expect("the channel decodes");
        channel["id"]
            .as_str()
            .expect("every team has an off-topic")
            .to_owned()
    };
    for user in [&go_user, &rs_user] {
        let removed = http
            .delete(format!(
                "{GO}/api/v4/channels/{off_topic}/members/{}",
                user.id
            ))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(removed.status().as_u16(), 200, "the guest left off-topic");
    }

    for user in [&go_user, &rs_user] {
        assert!(make_guest(&user.id).await, "the guest fixture is planted");
    }
    common::invalidate_go_caches(&http, &admin).await;

    for user in [&go_user, &rs_user] {
        let shape = guest_shape(&user.id).await.expect("the shape");
        assert_eq!(shape.0, "system_guest", "before: roles");
        assert!(shape.1 > 0, "before: at least one guest team membership");
        assert_eq!(shape.3, 0, "before: no channel membership is a user's");
    }

    let (go_status, go_body) = send(
        &http,
        GO,
        &admin,
        &format!("/api/v4/users/{}/promote", go_user.id),
        None,
    )
    .await;
    let (rs_status, rs_body) = send(
        &http,
        RUST,
        &admin,
        &format!("/api/v4/users/{}/promote", rs_user.id),
        None,
    )
    .await;
    assert_eq!(go_status, 200, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "us: {}", String::from_utf8_lossy(&rs_body));
    assert_eq!(go_body, rs_body, "both answer ReturnStatusOK verbatim");
    assert_eq!(String::from_utf8_lossy(&go_body), r#"{"status":"OK"}"#);

    // # A rejoined default channel comes back as a **guest** membership, on both servers
    //
    // `JoinDefaultChannels` is handed the `*model.User` the handler read *before* the promotion,
    // so the struct it adds the account with still says `system_guest` and the new
    // `ChannelMembers` row is written with `SchemeGuest = true` — after the transaction that had
    // just cleared every other one. Go's own staleness, reproduced because the rows are what a
    // client sees; measured here rather than reasoned about, which is why this assertion compares
    // the two servers instead of asserting zero.
    let go_shape = guest_shape(&go_user.id).await.expect("the shape");
    let rs_shape = guest_shape(&rs_user.id).await.expect("the shape");
    assert_eq!(
        rs_shape, go_shape,
        "the promoted accounts must end in the same shape",
    );
    for (user, shape) in [(&go_user, &go_shape), (&rs_user, &rs_shape)] {
        assert_eq!(shape.0, "system_user", "after: {} is a user", user.id);
        assert_eq!(shape.1, 0, "after: no team membership is a guest's");
        assert!(shape.3 > 0, "after: the channel memberships are a user's");
        assert_eq!(
            shape.4, 0,
            "after: `shouldBeAdmin` is false, so nothing rejoined as a channel admin",
        );

        // `JoinDefaultChannels` put the account back into `off-topic`.
        let rejoined = http
            .get(format!(
                "{GO}/api/v4/channels/{off_topic}/members/{}",
                user.id
            ))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(
            rejoined.status().as_u16(),
            200,
            "the promoted account rejoined the default channel",
        );
    }

    delete_plain_user(&http, &admin, &go_user.id).await;
    delete_plain_user(&http, &admin, &rs_user.id).await;
}

// ---------------------------------------------------------------------------------------------
// demoteUserToGuest
// ---------------------------------------------------------------------------------------------

/// **The licence is checked before the permission and before the user**, so on this deployment
/// every demote is the same 501 — including one aimed at an id that does not exist, by a caller
/// who holds nothing.
///
/// Only `RequireUserId` runs first, which is why `notanid` is still a 400.
#[tokio::test]
async fn demote_is_a_licence_refusal_before_anything_else() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "demogate").await;
    let user = create_plain_user(&http, &admin, &team, "demogate").await;

    same_refusal(
        &http,
        &admin,
        "/api/v4/users/notanid/demote",
        None,
        400,
        "api.context.invalid_url_param.app_error",
    )
    .await;

    for (token, path) in [
        (&user.token, format!("/api/v4/users/{MISSING}/demote")),
        (&user.token, format!("/api/v4/users/{}/demote", user.id)),
        (&admin, format!("/api/v4/users/{MISSING}/demote")),
        (&admin, "/api/v4/users/me/demote".to_owned()),
    ] {
        same_refusal(
            &http,
            token,
            &path,
            None,
            501,
            "api.team.demote_user_to_guest.license.error",
        )
        .await;
    }

    assert_eq!(
        guest_shape(&user.id).await.map(|shape| shape.0),
        Some("system_user".to_owned()),
        "five refusals must have demoted nobody",
    );

    delete_plain_user(&http, &admin, &user.id).await;
}

/// A licensed server is handed the whole request, before any read past the licence itself.
///
/// Planting `Systems.ActiveLicenseId` moves **this** side only — Go loaded its licence at startup
/// — so what this asserts is the forward: the answer comes back stamped `x-mmrs-served-by: go`
/// and is Go's own unlicensed 501. That is also why [D-511] exists: the licensed body cannot be
/// compared against Go on this stack at all.
#[tokio::test]
async fn a_licensed_demote_is_handed_to_go() {
    if !stack_enabled() {
        return;
    }
    let _licensed = ACTIVE_LICENCE_ROW.write().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let path = format!("/api/v4/users/{MISSING}/demote");

    common::set_active_licence_id(Some("mmrsdemotelicence000000000")).await;

    let response = http
        .post(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("mm-api answers");
    let status = response.status().as_u16();
    let forwarded = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("go");
    let body = response.bytes().await.expect("a body").to_vec();

    common::set_active_licence_id(None).await;

    assert!(forwarded, "a licensed demote must be forwarded");
    assert_eq!(status, 501, "which is Go's own unlicensed answer");
    let (go_status, go) = send(&http, GO, &admin, &path, None).await;
    assert_eq!(go_status, 501);
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("an error");
    assert_eq!(parsed["id"], "api.team.demote_user_to_guest.license.error");
    let ours: serde_json::Value = serde_json::from_slice(&body).expect("an error");
    assert_eq!(
        ours["id"], parsed["id"],
        "the forwarded body is Go's, not one we built",
    );
}

// ---------------------------------------------------------------------------------------------
// Which permission, not just "some permission"
// ---------------------------------------------------------------------------------------------

/// **`promote` wants `promote_guest`; the two conversions want `manage_system`** — and no stock
/// role on this deployment holds one without the other.
///
/// Every other refusal test here uses a plain user, who holds neither, so all four routes answer
/// 403 whichever permission they name. A role carrying exactly `promote_guest` separates them:
/// it gets *past* the promote gate — to the `no_guest` 501, which is proof of passage — and is
/// still refused by both conversions. Without it, swapping the two constants is invisible.
#[tokio::test]
async fn each_route_names_its_own_permission() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "cvtperm").await;
    let me = common::logged_in_user_id();

    let Some(role) = common::plant_role("cvtpromote", "promote_guest").await else {
        return; // no DATABASE_URL
    };
    let user = create_plain_user(&http, &admin, &team, "cvtperm").await;
    common::set_user_roles(&user.id, &format!("system_user {role}")).await;
    common::invalidate_go_caches(&http, &admin).await;
    // `session.Roles` is copied at login and never re-read, so the token has to be minted after
    // the grant or the grant is invisible to both servers.
    let token = common::login_plain_user(&http, "cvtperm").await;

    let bot = common::plant_bot("cvtperm", me, 0).await.expect("a bot");

    // Past the promote gate: `no_guest` is a 501 that only a caller who holds `promote_guest`
    // can ever see.
    same_refusal(
        &http,
        &token,
        &format!("/api/v4/users/{}/promote", user.id),
        None,
        501,
        "api.user.promote_guest_to_user.no_guest.app_error",
    )
    .await;

    // And refused by both conversions, which want `manage_system`.
    same_refusal(
        &http,
        &token,
        &format!("/api/v4/users/{}/convert_to_bot", user.id),
        None,
        403,
        "api.context.permissions.app_error",
    )
    .await;
    same_refusal(
        &http,
        &token,
        &format!("/api/v4/bots/{bot}/convert_to_user"),
        Some(br#"{"password":"Mmrs-Plain-1234"}"#),
        403,
        "api.context.permissions.app_error",
    )
    .await;

    common::unplant_bot(&bot).await;
    delete_plain_user(&http, &admin, &user.id).await;
}
