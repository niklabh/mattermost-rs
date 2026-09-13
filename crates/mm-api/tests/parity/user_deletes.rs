//! Cross-server parity for `DELETE /api/v4/users/{user_id}` and for the deactivation half of
//! `PUT /api/v4/users/{user_id}/active`, which is the same operation under a different verb.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity user_deletes
//! ```
//!
//! # This suite destroys accounts, so it only ever destroys its own
//!
//! Every subject is minted here under the prefix `mmrsdeluser`, which belongs to this file alone
//! and is swept by `common::purge_api_fixtures` at the start of the binary. **Nothing here
//! touches the fixture administrator the whole binary logs in as** — the last-system-admin guard
//! and the "target is a system admin" refusal are untestable on a shared stack for exactly that
//! reason ([D-462]), and are transcribed in the handler rather than asserted here.
//!
//! `common::USER_COUNT` is held by every test in the file. A soft delete moves
//! `Count(UserCountOptions{})`, whose first predicate is `DeleteAt = 0`, so a deactivation in
//! flight is an off-by-one in `users_stats` just as a creation is.
//!
//! # There is no websocket assertion here, deliberately
//!
//! `updateUserActive` publishes `model.NewWebSocketEvent(user_activation_status_change, "", "",
//! "", nil, "")` — no team, no channel, no user, no omit-list. The frame carries **nothing that
//! identifies whose activation changed**, so a wait on it cannot be scoped to this suite's
//! subject and would be satisfied by any other suite's deactivation. An unscoped wait that
//! returns on a stranger's frame is a test that passes for the wrong reason; see [D-474].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, go_minted_token, stack_enabled,
};

const PASSWORD: &str = "Mmrs-Del-1234";

fn username(tag: &str) -> String {
    format!("mmrsdeluser{tag}")
}

fn email(tag: &str) -> String {
    format!("{}@mmrs.invalid", username(tag))
}

async fn pool() -> Option<sqlx::PgPool> {
    common::fixture_pool().await
}

/// Remove every trace of an account this suite created — hard, not soft, because the route under
/// test leaves a *soft*-deleted row that `users_stats` still counts.
async fn scrub(tag: &str) {
    let Some(pool) = pool().await else {
        return;
    };
    let username = username(tag);
    for statement in [
        "DELETE FROM bots WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM bots WHERE ownerid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM oauthauthdata WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM oauthaccessdata WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM preferences WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM sessions WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM teammembers WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM channelmembers WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM status WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM users WHERE username = $1",
    ] {
        let _ = sqlx::query(statement).bind(&username).execute(&pool).await;
    }
}

async fn scrub_pair(tag: &str) {
    scrub(&format!("{tag}go")).await;
    scrub(&format!("{tag}rs")).await;
}

/// One stored column of one account, by username. Every assertion about persisted state in this
/// file is scoped to a row this test created; nothing here counts anything global.
async fn column_of(tag: &str, column: &str) -> Option<String> {
    let pool = pool().await?;
    // The column name is a literal from this file, never from a test input.
    let sql = format!("SELECT {column}::text FROM users WHERE username = $1");
    sqlx::query_scalar::<_, Option<String>>(&sql)
        .bind(username(tag))
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten()
        .flatten()
}

/// `DeleteAt`, as an integer, for one of this suite's accounts.
async fn delete_at(tag: &str) -> i64 {
    column_of(tag, "deleteat")
        .await
        .unwrap_or_default()
        .parse()
        .unwrap_or_default()
}

/// How many session rows one of this suite's accounts still has. Scoped by username, so another
/// suite's logins are invisible to it.
async fn session_count(tag: &str) -> i64 {
    let Some(pool) = pool().await else {
        return 0;
    };
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM sessions
          WHERE userid IN (SELECT id FROM users WHERE username = $1)",
    )
    .bind(username(tag))
    .fetch_one(&pool)
    .await
    .unwrap_or_default()
}

/// Rows in one of the two OAuth tables belonging to one of this suite's accounts.
async fn oauth_rows(tag: &str, table: &str) -> i64 {
    let Some(pool) = pool().await else {
        return 0;
    };
    // The table name is one of two literals from the call sites below.
    let sql = format!(
        "SELECT COUNT(*) FROM {table}
          WHERE userid IN (SELECT id FROM users WHERE username = $1)"
    );
    sqlx::query_scalar::<_, i64>(&sql)
        .bind(username(tag))
        .fetch_one(&pool)
        .await
        .unwrap_or_default()
}

/// Give an account one outstanding authorization code **and** one live access token.
///
/// Neither can be minted through the API — nothing migrated performs an OAuth flow and
/// `EnableOAuthServiceProvider` is off — so the two rows are planted. They are plain rows in the
/// shape Go's own `SaveAuthData`/`SaveAccessData` write, and what is under test is the pair of
/// `DELETE`s `userDeactivated` runs over them, not how they got there.
async fn plant_oauth_grants(tag: &str, seed: &str) {
    let Some(pool) = pool().await else {
        return;
    };
    let id = match column_of(tag, "id").await {
        Some(id) => id,
        None => return,
    };
    let _ = sqlx::query(
        "INSERT INTO oauthauthdata
             (clientid, userid, code, expiresin, createat, redirecturi, state, scope)
         VALUES ($1, $2, $3, 3600, 1, 'http://localhost/', '', '')
         ON CONFLICT (code) DO NOTHING",
    )
    .bind(format!("mmrsdelclient{seed:0>13}"))
    .bind(&id)
    .bind(format!("mmrsdelcode{seed:0>15}"))
    .execute(&pool)
    .await;
    let _ = sqlx::query(
        "INSERT INTO oauthaccessdata
             (token, refreshtoken, redirecturi, clientid, userid, expiresat, scope)
         VALUES ($1, $2, 'http://localhost/', $3, $4, 0, '')
         ON CONFLICT (token) DO NOTHING",
    )
    .bind(format!("mmrsdeltoken{seed:0>14}"))
    .bind(format!("mmrsdelrefr{seed:0>15}"))
    .bind(format!("mmrsdelclient{seed:0>13}"))
    .bind(&id)
    .execute(&pool)
    .await;
}

/// Make `<owner_tag>` the owner of a bot account, so `App::owns_bots` answers true.
///
/// `POST /api/v4/bots` is a 403 on this stack (the bot-accounts feature flag is off — see
/// `parity/bot_writes.rs`), so the `Bots` row is planted over a real `Users` row this suite
/// minted. What the forward decision reads is exactly this row.
async fn plant_bot(owner_tag: &str, bot_tag: &str, deleted_at: i64) {
    let Some(pool) = pool().await else {
        return;
    };
    let (Some(owner), Some(bot)) = (
        column_of(owner_tag, "id").await,
        column_of(bot_tag, "id").await,
    ) else {
        return;
    };
    let _ = sqlx::query(
        "INSERT INTO bots (userid, description, ownerid, createat, updateat, deleteat,
                           lasticonupdate)
         VALUES ($1, 'a bot this suite owns', $2, 1, 1, $3, 0)
         ON CONFLICT (userid) DO UPDATE
             SET ownerid = EXCLUDED.ownerid, deleteat = EXCLUDED.deleteat",
    )
    .bind(&bot)
    .bind(&owner)
    .bind(deleted_at)
    .execute(&pool)
    .await;
}

async fn bot_delete_at(bot_tag: &str) -> i64 {
    let Some(pool) = pool().await else {
        return 0;
    };
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT b.deleteat FROM bots b
           JOIN users u ON u.id = b.userid
          WHERE u.username = $1",
    )
    .bind(username(bot_tag))
    .fetch_optional(&pool)
    .await
    .ok()
    .flatten()
    .flatten()
    .unwrap_or_default()
}

/// Create one account through Go's admin API, then put it on a team.
async fn make_user(http: &reqwest::Client, admin: &str, team_id: &str, tag: &str) -> String {
    scrub(tag).await;
    let response = http
        .post(format!("{GO}/api/v4/users"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "email": email(tag),
            "username": username(tag),
            "password": PASSWORD,
            "nickname": "DelNick",
            "first_name": "DelFirst",
            "last_name": "DelLast",
            "position": "DelPosition",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating {} failed: {}",
        username(tag),
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the user decodes");
    let id = created["id"].as_str().expect("an id").to_owned();

    let joined = http
        .post(format!("{GO}/api/v4/teams/{team_id}/members"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "team_id": team_id, "user_id": id }))
        .send()
        .await
        .expect("Go answers");
    assert!(joined.status().is_success(), "team join failed");

    id
}

/// The pair of subjects a comparison needs: `<tag>go` and `<tag>rs`, identical but for identity.
async fn pair(http: &reqwest::Client, admin: &str, team_id: &str, tag: &str) -> (String, String) {
    let go = make_user(http, admin, team_id, &format!("{tag}go")).await;
    let rs = make_user(http, admin, team_id, &format!("{tag}rs")).await;
    (go, rs)
}

async fn login(http: &reqwest::Client, tag: &str) -> String {
    let response = http
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username(tag), "password": PASSWORD }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 200, "{} cannot log in", username(tag));
    response
        .headers()
        .get("token")
        .expect("a token header")
        .to_str()
        .expect("ASCII")
        .to_owned()
}

/// A `DELETE` against one base, returning the status, the raw body and `x-mmrs-served-by`.
async fn delete(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
) -> (u16, Vec<u8>, Option<String>) {
    let response = http
        .delete(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = response.bytes().await.expect("a body").to_vec();
    (status, bytes, served)
}

/// A `PUT` against one base, same shape.
async fn put(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: &str,
    body: &[u8],
) -> (u16, Vec<u8>, Option<String>) {
    let response = http
        .put(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = response.bytes().await.expect("a body").to_vec();
    (status, bytes, served)
}

async fn admin_and_team(http: &reqwest::Client) -> (String, String) {
    let admin = go_minted_token(http).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(http, &admin).await;
    (admin, team_id)
}

// ---------------------------------------------------------------------------------------------
// The soft delete
// ---------------------------------------------------------------------------------------------

/// **The served soft delete matches Go byte for byte, and leaves the same row behind.**
///
/// The response is `{"status":"OK"}` on both sides, so most of the evidence is in the database:
/// `DeleteAt` is non-zero, `DeleteAt == UpdateAt` exactly — one `GetMillis()` feeds both
/// (app/user.go:1244), and reading the clock twice would leave them a millisecond apart — and the
/// session the account had is gone, which is `RevokeAllSessions`.
#[tokio::test]
async fn a_soft_delete_agrees_and_is_served_here() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "soft").await;
    let _ = login(&http, "softgo").await;
    let _ = login(&http, "softrs").await;
    assert_eq!(
        session_count("softrs").await,
        1,
        "there is a session to revoke"
    );

    // An **already-disabled** bot owned by the subject. `notifySysadminsBotOwnerDeactivated` and
    // `disableUserBots` both page with `IncludeDeleted: false`, so this bot is invisible to them
    // and the account still counts as owning none — which is why this request is served rather
    // than forwarded. Without this row, widening the forward gate to include deleted bots would
    // be indistinguishable from not widening it.
    make_user(&http, &admin, &team, "softdeadbot").await;
    plant_bot("softrs", "softdeadbot", 1).await;

    let (go_status, go_body, _) =
        delete(&http, GO, &format!("/api/v4/users/{go_id}"), &admin).await;
    let (rs_status, rs_body, served) =
        delete(&http, RUST, &format!("/api/v4/users/{rs_id}"), &admin).await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(rs_body, go_body, "the two bodies are identical bytes");
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "served here, not forwarded"
    );

    for tag in ["softgo", "softrs"] {
        let deleted = delete_at(tag).await;
        let updated: i64 = column_of(tag, "updateat")
            .await
            .unwrap_or_default()
            .parse()
            .unwrap_or_default();
        assert_ne!(deleted, 0, "{tag} is soft-deleted");
        assert!(
            (0..=5).contains(&(updated - deleted)),
            "{tag}: DeleteAt is seeded from an UpdateAt that PreUpdate immediately re-stamps, \
             so DeleteAt <= UpdateAt and the two are milliseconds apart — got {deleted} and \
             {updated}"
        );
        assert_eq!(
            session_count(tag).await,
            0,
            "{tag}: every session is revoked"
        );
    }

    scrub_pair("soft").await;
    scrub("softdeadbot").await;
}

/// **Both OAuth tables are swept, and only for the account being deactivated.**
///
/// `userDeactivated` runs two deletes whose names do not say which table each touches:
/// `RemoveAuthDataByUserId` clears `OAuthAuthData` (authorization codes) and
/// `PermanentDeleteAuthDataByUser` — despite the name — clears **`OAuthAccessData`** (access
/// tokens). A port that read the names instead of the statements swaps them and leaves every
/// live OAuth token of a deactivated account working.
///
/// The bystander is what makes the `WHERE userid = $1` load-bearing: an unpredicated delete
/// would empty both tables and every other assertion here would still pass.
#[tokio::test]
async fn a_soft_delete_clears_both_oauth_tables_and_only_for_that_user() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "oauth").await;
    make_user(&http, &admin, &team, "oauthbys").await;

    plant_oauth_grants("oauthgo", "1").await;
    plant_oauth_grants("oauthrs", "2").await;
    plant_oauth_grants("oauthbys", "3").await;
    for tag in ["oauthgo", "oauthrs", "oauthbys"] {
        assert_eq!(
            oauth_rows(tag, "oauthauthdata").await,
            1,
            "{tag} has a code"
        );
        assert_eq!(
            oauth_rows(tag, "oauthaccessdata").await,
            1,
            "{tag} has a token"
        );
    }

    let (go_status, go_body, _) =
        delete(&http, GO, &format!("/api/v4/users/{go_id}"), &admin).await;
    let (rs_status, rs_body, served) =
        delete(&http, RUST, &format!("/api/v4/users/{rs_id}"), &admin).await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "served here, not forwarded"
    );

    for tag in ["oauthgo", "oauthrs"] {
        assert_eq!(
            oauth_rows(tag, "oauthauthdata").await,
            0,
            "{tag}: RemoveAuthDataByUserId cleared OAuthAuthData"
        );
        assert_eq!(
            oauth_rows(tag, "oauthaccessdata").await,
            0,
            "{tag}: PermanentDeleteAuthDataByUser cleared OAuthAccessData — that is the table \
             its name does *not* say"
        );
    }
    assert_eq!(
        oauth_rows("oauthbys", "oauthauthdata").await,
        1,
        "the bystander's code survives — the delete is predicated on the user"
    );
    assert_eq!(
        oauth_rows("oauthbys", "oauthaccessdata").await,
        1,
        "and so does the bystander's token"
    );

    scrub_pair("oauth").await;
    scrub("oauthbys").await;
}

/// **A plain user cannot delete somebody else's account**, and the two servers say so identically.
#[tokio::test]
async fn a_plain_user_cannot_delete_another_account() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "perm").await;
    make_user(&http, &admin, &team, "permcaller").await;
    let caller = login(&http, "permcaller").await;

    let (go_status, go_body, _) =
        delete(&http, GO, &format!("/api/v4/users/{go_id}"), &caller).await;
    let (rs_status, rs_body, served) =
        delete(&http, RUST, &format!("/api/v4/users/{rs_id}"), &caller).await;

    assert_eq!(go_status, 403, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "the refusal is served here"
    );
    let go =
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "DELETE as a plain user");
    assert_eq!(go["id"], "api.context.permissions.app_error");

    assert_eq!(delete_at("permgo").await, 0, "nothing was written");
    assert_eq!(delete_at("permrs").await, 0, "nothing was written");

    scrub_pair("perm").await;
    scrub("permcaller").await;
}

/// **A self-delete is refused while `TeamSettings.EnableUserDeactivation` is off**, which is what
/// the live document says on this stack.
///
/// The caller here holds no `manage_system`, so the guard's third conjunct
/// (`!SessionHasPermissionTo(ManageSystem)`) is true and the refusal fires. A system admin
/// deleting *themselves* would pass it — the escape `updateUserActive`'s otherwise identical
/// guard does not have — and is not tried here, because the only system admin on this stack is
/// the account the whole binary logs in as.
#[tokio::test]
async fn a_self_delete_is_refused_while_the_deactivation_flag_is_off() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "self").await;
    let go_token = login(&http, "selfgo").await;
    let rs_token = login(&http, "selfrs").await;

    let (go_status, go_body, _) =
        delete(&http, GO, &format!("/api/v4/users/{go_id}"), &go_token).await;
    let (rs_status, rs_body, served) =
        delete(&http, RUST, &format!("/api/v4/users/{rs_id}"), &rs_token).await;

    assert_eq!(go_status, 401, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "the refusal is served here"
    );
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "self DELETE");
    assert_eq!(go["id"], "api.user.update_active.not_enable.app_error");

    for tag in ["selfgo", "selfrs"] {
        assert_eq!(delete_at(tag).await, 0, "{tag}: nothing was written");
        assert_eq!(session_count(tag).await, 1, "{tag}: the session survives");
    }

    // `me` resolves to the session owner before the same guard, so it is the same refusal.
    let (me_status, me_body, me_served) = delete(&http, RUST, "/api/v4/users/me", &rs_token).await;
    assert_eq!(me_status, 401, "{}", String::from_utf8_lossy(&me_body));
    assert_eq!(me_served.as_deref(), Some("rust"));
    assert_eq!(
        delete_at("selfrs").await,
        0,
        "and `me` wrote nothing either"
    );

    scrub_pair("self").await;
}

// ---------------------------------------------------------------------------------------------
// ?permanent=true
// ---------------------------------------------------------------------------------------------

/// **`?permanent=true` is refused with the system-admin wording, and writes nothing.**
///
/// `ServiceSettings.EnableAPIUserDeletion` is off in the live document, so the arm that would run
/// `App.PermanentDeleteUser` is unreachable here and the refusal is what a client gets. The
/// caller is the fixture administrator, so this is the `for_admin` fork — Go's "More verbose
/// error message for system admins".
///
/// The `DeleteAt` assertion is the one that matters: a refusal that had already soft-deleted the
/// row would answer with exactly this body.
#[tokio::test]
async fn a_permanent_delete_is_refused_with_the_admin_wording_and_writes_nothing() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "hard").await;

    let (go_status, go_body, _) = delete(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}?permanent=true"),
        &admin,
    )
    .await;
    let (rs_status, rs_body, served) = delete(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}?permanent=true"),
        &admin,
    )
    .await;

    assert_eq!(go_status, 401, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "the refusal is served here"
    );
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "permanent DELETE");
    assert_eq!(
        go["id"],
        "api.user.delete_user.not_enabled.for_admin.app_error"
    );

    for tag in ["hardgo", "hardrs"] {
        assert_eq!(delete_at(tag).await, 0, "{tag}: the refusal wrote nothing");
        assert!(
            column_of(tag, "id").await.is_some(),
            "{tag}: and the row is still there"
        );
    }

    scrub_pair("hard").await;
}

/// **`permanent` is `strconv.ParseBool` with the error discarded, so a misspelling soft-deletes.**
///
/// `?permanent=yes` is not a bool Go recognises, the error is thrown away, and the request
/// becomes an ordinary deactivation answered `{"status":"OK"}` — indistinguishable from the
/// permanent delete the caller asked for. `?permanent=t` is one of the six true spellings and
/// reaches the refusal. Both halves are asserted, because a port that treated any present
/// `permanent` key as true would pass the second and fail the first, and one that used
/// `"true".eq_ignore_ascii_case` would pass both while accepting `tRue`, which Go rejects.
#[tokio::test]
async fn permanent_is_parsed_like_strconv_parsebool() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (yes_go, yes_rs) = pair(&http, &admin, &team, "spell").await;
    let (t_go, t_rs) = pair(&http, &admin, &team, "short").await;

    // `yes` is not a bool → false → a soft delete.
    let (go_status, go_body, _) = delete(
        &http,
        GO,
        &format!("/api/v4/users/{yes_go}?permanent=yes"),
        &admin,
    )
    .await;
    let (rs_status, rs_body, served) = delete(
        &http,
        RUST,
        &format!("/api/v4/users/{yes_rs}?permanent=yes"),
        &admin,
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(rs_body, go_body);
    assert_eq!(served.as_deref(), Some("rust"));
    assert_ne!(delete_at("spellgo").await, 0, "Go soft-deleted it");
    assert_ne!(delete_at("spellrs").await, 0, "and so did we");

    // `t` is one of the six → true → the refusal.
    let (go_status, go_body, _) = delete(
        &http,
        GO,
        &format!("/api/v4/users/{t_go}?permanent=t"),
        &admin,
    )
    .await;
    let (rs_status, rs_body, served) = delete(
        &http,
        RUST,
        &format!("/api/v4/users/{t_rs}?permanent=t"),
        &admin,
    )
    .await;
    assert_eq!(go_status, 401, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    assert_eq!(served.as_deref(), Some("rust"));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "?permanent=t");
    assert_eq!(delete_at("shortgo").await, 0, "and neither server wrote");
    assert_eq!(delete_at("shortrs").await, 0);

    scrub_pair("spell").await;
    scrub_pair("short").await;
}

// ---------------------------------------------------------------------------------------------
// The bot-owner forward
// ---------------------------------------------------------------------------------------------

/// **Deleting an account that owns a bot is forwarded, and the forward precedes the write.**
///
/// The bot half of `userDeactivated` — `notifySysadminsBotOwnerDeactivated`'s DM to every system
/// administrator and `disableUserBots`' cascade — is the only part this process cannot reproduce,
/// and both are gated on the same `SELECT`. Go's `Bots.DeleteAt` moving is the evidence that the
/// forward happened *before* anything was written here: nothing in this port disables a bot, so a
/// deactivation that had been served would leave that column at zero.
#[tokio::test]
async fn deleting_a_bot_owner_forwards_before_any_write() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let owner_id = make_user(&http, &admin, &team, "botowner").await;
    make_user(&http, &admin, &team, "botaccount").await;
    plant_bot("botowner", "botaccount", 0).await;
    assert_eq!(
        bot_delete_at("botaccount").await,
        0,
        "the bot starts enabled"
    );

    let (status, body, served) =
        delete(&http, RUST, &format!("/api/v4/users/{owner_id}"), &admin).await;

    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_ne!(
        served.as_deref(),
        Some("rust"),
        "an owner of bots must be forwarded, not served here"
    );
    assert_ne!(delete_at("botowner").await, 0, "Go did the deactivation");
    assert_ne!(
        bot_delete_at("botaccount").await,
        0,
        "and disableUserBots ran, which only Go can do"
    );

    scrub("botowner").await;
    scrub("botaccount").await;
}

// ---------------------------------------------------------------------------------------------
// PUT /users/{id}/active — the deactivation half
// ---------------------------------------------------------------------------------------------

/// **An administrator deactivating somebody else through `/active` is served**, and agrees with
/// Go on the body and on the row.
///
/// This is the closure of [D-461]: the whole request used to be forwarded the moment the body
/// read `"active": false`. What made it servable is that the two unreproducible steps are no-ops
/// for an owner of no bots, and that is knowable before the `UPDATE`.
#[tokio::test]
async fn deactivating_a_bot_less_account_is_served_and_revokes_its_sessions() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "deac").await;
    let _ = login(&http, "deacgo").await;
    let _ = login(&http, "deacrs").await;
    assert_eq!(
        session_count("deacrs").await,
        1,
        "there is a session to revoke"
    );

    let (go_status, go_body, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/active"),
        &admin,
        br#"{"active":false}"#,
    )
    .await;
    let (rs_status, rs_body, served) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/active"),
        &admin,
        br#"{"active":false}"#,
    )
    .await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(rs_body, go_body);
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "served here, not forwarded"
    );

    for tag in ["deacgo", "deacrs"] {
        let deleted = delete_at(tag).await;
        let updated: i64 = column_of(tag, "updateat")
            .await
            .unwrap_or_default()
            .parse()
            .unwrap_or_default();
        assert_ne!(deleted, 0, "{tag} is deactivated");
        assert!(
            (0..=5).contains(&(updated - deleted)),
            "{tag}: DeleteAt is seeded from an UpdateAt that PreUpdate immediately re-stamps, \
             so DeleteAt <= UpdateAt and the two are milliseconds apart — got {deleted} and \
             {updated}"
        );
        assert_eq!(
            session_count(tag).await,
            0,
            "{tag}: every session is revoked"
        );
    }

    scrub_pair("deac").await;
}

/// **`/active` and `DELETE` disagree about a system admin deactivating themselves, and this pins
/// the half that is reachable.**
///
/// `updateUserActive`'s self-deactivation guard is `isSelfDeactivate && !EnableUserDeactivation`
/// with **no** `manage_system` escape (api4/user.go:1918), where `deleteUser`'s has one
/// (api4/user.go:1684). The flag is off on this stack, so a plain user gets a 401 from both
/// routes and the difference only shows for a system admin — which this stack has exactly one of
/// and it is the account the binary logs in as. The refusal itself is asserted here; the
/// asymmetry is transcribed in the two handlers.
#[tokio::test]
async fn a_self_deactivation_is_refused_while_the_flag_is_off() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let (go_id, rs_id) = pair(&http, &admin, &team, "sdea").await;
    let go_token = login(&http, "sdeago").await;
    let rs_token = login(&http, "sdears").await;

    let (go_status, go_body, _) = put(
        &http,
        GO,
        &format!("/api/v4/users/{go_id}/active"),
        &go_token,
        br#"{"active":false}"#,
    )
    .await;
    let (rs_status, rs_body, served) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{rs_id}/active"),
        &rs_token,
        br#"{"active":false}"#,
    )
    .await;

    assert_eq!(go_status, 401, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, go_status);
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "the refusal is served here"
    );
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "self deactivation");
    assert_eq!(go["id"], "api.user.update_active.not_enable.app_error");

    for tag in ["sdeago", "sdears"] {
        assert_eq!(delete_at(tag).await, 0, "{tag}: nothing was written");
        assert_eq!(session_count(tag).await, 1, "{tag}: the session survives");
    }

    scrub_pair("sdea").await;
}

/// **The bot-owner forward is on `/active` too**, and for the same reason.
///
/// Registered separately from the `DELETE` case because the two handlers ask the question at
/// different points — `deleteUser` immediately after the system-admin gate, `updateUserActive`
/// after four more gates including the LDAP one — and a port that added the check to one and not
/// the other would pass the other test.
#[tokio::test]
async fn deactivating_a_bot_owner_forwards_before_any_write() {
    if !stack_enabled() {
        return;
    }
    let _count = common::USER_COUNT.lock().await;
    let http = client();
    let (admin, team) = admin_and_team(&http).await;
    let owner_id = make_user(&http, &admin, &team, "adeacowner").await;
    make_user(&http, &admin, &team, "adeacbot").await;
    plant_bot("adeacowner", "adeacbot", 0).await;
    assert_eq!(bot_delete_at("adeacbot").await, 0, "the bot starts enabled");

    let (status, body, served) = put(
        &http,
        RUST,
        &format!("/api/v4/users/{owner_id}/active"),
        &admin,
        br#"{"active":false}"#,
    )
    .await;

    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_ne!(
        served.as_deref(),
        Some("rust"),
        "an owner of bots must be forwarded, not served here"
    );
    assert_ne!(delete_at("adeacowner").await, 0, "Go did the deactivation");
    assert_ne!(
        bot_delete_at("adeacbot").await,
        0,
        "and disableUserBots ran, which only Go can do"
    );

    scrub("adeacowner").await;
    scrub("adeacbot").await;
}
