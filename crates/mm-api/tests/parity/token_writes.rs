//! Cross-server parity for the seven personal-access-token **writes**: `createUserAccessToken`
//! (api4/user.go:2970), `revokeUserAccessToken` (:3215), `disableUserAccessToken` (:3265),
//! `enableUserAccessToken` (:3316), `rotateUserAccessToken` (:3367), `searchUserAccessTokens`
//! (:3046) and `revokeNonCompliantUserAccessTokens` (:3126).
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity token_writes
//! ```
//!
//! # These are writes, so `post_both_raw` is only safe on the refusals
//!
//! It posts the *same* body to both servers. For a refusal that is exactly what is wanted — two
//! servers, one request, compare the errors. For anything that succeeds it is not: Go revokes the
//! token and we then 404 on a row that is already gone. So every success case here plants **two**
//! equivalent fixtures, sends one to each server, and compares the answers after blanking the two
//! fields that cannot match (`id` and `token` are freshly minted 26-character ids).
//!
//! # The feature is off, and bots are the way in
//!
//! `ServiceSettings.EnableUserAccessTokens` is `false` on this deployment, so
//! `CreateUserAccessToken` and `RotateUserAccessToken` answer **501** for a human. Go exempts bot
//! accounts from that check (`!enabled && !user.IsBot`), so a planted bot exercises the whole
//! create and rotate path with the stock configuration untouched. The 501 itself is asserted too —
//! it is what a human client actually gets here.
//!
//! # Nothing here asserts that Go is stale
//!
//! Revoking a token deletes the session it minted, and Go serves sessions from a cache this port
//! does not have. A test claiming "revoked here, still accepted there" would be racing every other
//! suite's cache invalidation, so the DB effects are asserted against the rows instead.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, assert_error_bodies_match_except_known_gaps, client, go_minted_token,
    logged_in_user_id, post_both_raw, stack_enabled,
};

use super::user_access_tokens::{TOKENS, plant_token, secret_for, unplant_tokens};

/// POST one body to **one** server, returning `(status, body)`.
///
/// `common::post_both_raw` sends the same request to both, which is right for a refusal and wrong
/// for a write: the second server would be acting on a row the first has already revoked. Every
/// success case here plants a fixture per server and drives them one at a time through this.
async fn post_one(base: &str, token: &str, path: &str, body: &str) -> (u16, Vec<u8>) {
    let response = client()
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    if base == common::RUST {
        // Without this a forwarded response would be compared against Go's own and pass for the
        // wrong reason — the failure mode `assert_served_by_rust` exists for.
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.bytes().await.expect("body reads").to_vec())
}

/// [`post_one`] with the `{"token_id": …}` body the three lifecycle routes take.
async fn post_token_id(base: &str, token: &str, path: &str, token_id: &str) -> (u16, Vec<u8>) {
    post_one(
        base,
        token,
        path,
        &format!(r#"{{"token_id":"{token_id}"}}"#),
    )
    .await
}

/// The owner of every token this suite creates through the route rather than planting:
/// `seed-bot`, the stack's own seeded bot (`scripts/stack.sh:106`).
///
/// A bot because bots are exempt from `EnableUserAccessTokens` — see the module note. **Seeded
/// rather than planted**: `common::plant_bot` writes an `mmrsbot%` id, and the bots suite sweeps
/// that prefix wholesale, so a bot of ours could vanish between the plant and the POST that needs
/// it. `seed-bot` belongs to the stack, is swept by nothing, and is created before any test runs.
const TOKEN_BOT: &str = "seedbotdescribed0000000000";

/// Remove everything this suite writes: the planted rows (`mmrstok%`), the rows the create and
/// rotate routes minted with random ids, and any session they authenticated.
///
/// Swept by **description**, because a token created through the route has an id we did not
/// choose. Every body this suite posts carries a description starting `mmrs-write`.
async fn sweep() {
    unplant_tokens().await;
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    sqlx::query(
        "DELETE FROM sessions WHERE token IN
             (SELECT token FROM useraccesstokens WHERE description LIKE 'mmrs-write%')",
    )
    .execute(&pool)
    .await
    .expect("the sessions go first");
    sqlx::query("DELETE FROM useraccesstokens WHERE description LIKE 'mmrs-write%'")
        .execute(&pool)
        .await
        .expect("the created tokens are removed");
    // The planted sessions, including the bystander no write is supposed to touch.
    sqlx::query("DELETE FROM sessions WHERE id LIKE 'mmrssess%'")
        .execute(&pool)
        .await
        .expect("the planted sessions are removed");
}

/// One token row as the database holds it — the oracle for "did the write land".
#[derive(Debug, PartialEq, Eq)]
struct Row {
    is_active: bool,
    expires_at: i64,
    token: String,
}

async fn row(token_id: &str) -> Option<Row> {
    let pool = common::fixture_pool().await?;
    sqlx::query_as::<_, (bool, i64, String)>(
        "SELECT isactive, expiresat, token FROM useraccesstokens WHERE id = $1",
    )
    .bind(token_id)
    .fetch_optional(&pool)
    .await
    .expect("the row reads")
    .map(|(is_active, expires_at, token)| Row {
        is_active,
        expires_at,
        token,
    })
}

/// Plant a session authenticated by a planted token's secret, the way Go's own
/// `GetSession` → access-token path does: `Sessions.Token` **is** the secret.
///
/// This is what the three session-sweeping writes are supposed to delete, and the only way to set
/// one up without minting a real session against a token this suite controls.
async fn plant_session_for(tag: &str, user_id: &str) -> bool {
    let Some(pool) = common::fixture_pool().await else {
        return false;
    };
    sqlx::query(
        "INSERT INTO sessions (id, token, createat, expiresat, lastactivityat, userid, deviceid,
                               roles, isoauth, props, expirednotify)
         VALUES ($1, $2, 1788600000000, 0, 1788600000000, $3, '', 'system_user', false,
                 '{}'::jsonb, false)
         ON CONFLICT (id) DO UPDATE SET token = EXCLUDED.token",
    )
    .bind(format!("mmrssess{tag:0>18}"))
    .bind(secret_for(tag))
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("the session row is written");
    true
}

/// [`plant_session_for`] against an arbitrary secret rather than a planted tag's — needed for a
/// token the *route* created, whose secret we learn from the response.
async fn plant_session_on_secret(tag: &str, secret: &str) -> bool {
    let Some(pool) = common::fixture_pool().await else {
        return false;
    };
    sqlx::query(
        "INSERT INTO sessions (id, token, createat, expiresat, lastactivityat, userid, deviceid,
                               roles, isoauth, props, expirednotify)
         VALUES ($1, $2, 1788600000000, 0, 1788600000000, $3, '', 'system_user', false,
                 '{}'::jsonb, false)
         ON CONFLICT (id) DO UPDATE SET token = EXCLUDED.token",
    )
    .bind(format!("mmrssess{tag:0>18}"))
    .bind(secret)
    .bind(TOKEN_BOT)
    .execute(&pool)
    .await
    .expect("the session row is written");
    true
}

async fn session_exists_with_secret(secret: &str) -> bool {
    let Some(pool) = common::fixture_pool().await else {
        return false;
    };
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions WHERE token = $1")
        .bind(secret)
        .fetch_one(&pool)
        .await
        .expect("the count reads")
        > 0
}

async fn session_exists(tag: &str) -> bool {
    let Some(pool) = common::fixture_pool().await else {
        return false;
    };
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions WHERE token = $1")
        .bind(secret_for(tag))
        .fetch_one(&pool)
        .await
        .expect("the count reads")
        > 0
}

/// A token document with the two unpredictable fields blanked, so two servers' answers to the
/// same request are comparable.
///
/// **`id` and `token` are asserted present and 26 characters before being dropped**, which is the
/// half that matters: blanking a field nobody checked would hide exactly the bug this suite is
/// looking for.
fn comparable(body: &[u8], context: &str) -> serde_json::Value {
    let mut value: serde_json::Value =
        serde_json::from_slice(body).unwrap_or_else(|e| panic!("{context}: not JSON: {e}"));
    let object = value.as_object_mut().expect("an object");

    for key in ["id", "token"] {
        let minted = object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("{context}: no `{key}` in {object:?}"));
        assert_eq!(
            minted.len(),
            26,
            "{context}: `{key}` must be a freshly minted 26-character id, not {minted:?}"
        );
        object.insert(key.to_owned(), serde_json::Value::Null);
    }
    value
}

/// A human is refused with **501**, not 403 — `EnableUserAccessTokens` is off and the refusal is
/// "not implemented on this server". The id has no `.app_error` suffix, which nothing else in the
/// family does either.
#[tokio::test]
async fn creating_a_token_for_a_human_is_a_501_while_the_feature_is_off() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();

    let path = format!("/api/v4/users/{me}/tokens");
    let body = br#"{"description":"mmrs-write disabled"}"#;
    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, &path, body).await;

    assert_eq!(go_status, 501, "Go refuses a human");
    assert_eq!(rs_status, go_status);
    let go_body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(go_body["id"], "app.user_access_token.disabled");

    sweep().await;
}

/// The body decode, in Go's order and with Go's parameter names. Every one of these was measured
/// against the running server rather than reasoned about, because three of them are
/// counter-intuitive:
///
/// - `[]` is a **decode failure** and names the handler's own parameter, where `serde` would
///   happily fill the struct positionally;
/// - `null` is **not** a decode failure — the struct keeps its zero value, so the refusal is the
///   empty `description` instead;
/// - the user is fetched **before** the body is read, so a bad body against an unknown user is a
///   404 rather than a 400.
#[tokio::test]
async fn a_malformed_create_body_names_the_parameter_go_names() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let path = format!("/api/v4/users/{me}/tokens");

    for (body, name) in [
        (&b"[]"[..], "user_access_token"),
        (b"", "user_access_token"),
        (b"\"x\"", "user_access_token"),
        (b"null", "description"),
        (b"{}", "description"),
        (br#"{"description":""}"#, "description"),
    ] {
        let context = format!("{path} <- {}", String::from_utf8_lossy(body));
        let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, &path, body).await;
        assert_eq!(go_status, 400, "{context}");
        assert_eq!(rs_status, go_status, "{context}");
        let go_body = assert_error_bodies_match_except_known_gaps(&go, &rs, &context);
        assert_eq!(
            go_body["id"], "api.context.invalid_body_param.app_error",
            "{context}"
        );
        // **The parameter name is the only thing separating these two branches**, and it is the
        // one the shared-body helper is allowed to skip: it lives in `message`, which differs
        // between the servers for i18n reasons. So assert it against **Go's** message, which is
        // the oracle for which branch Go took.
        assert!(
            go_body["message"]
                .as_str()
                .is_some_and(|m| m.contains(name)),
            "{context}: Go's message must name `{name}`, got {}",
            go_body["message"]
        );
    }

    // The decode never runs for a user that does not exist.
    let missing = "/api/v4/users/zzzzzzzzzzzzzzzzzzzzzzzzzz/tokens";
    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, missing, b"[]").await;
    assert_eq!(
        go_status, 404,
        "the user is fetched before the body is read"
    );
    assert_eq!(rs_status, go_status);
    let go_body = assert_error_bodies_match_except_known_gaps(&go, &rs, missing);
    assert_eq!(go_body["id"], "app.user.missing_account.const");

    sweep().await;
}

/// The one response that carries a live secret, on both servers.
///
/// The bodies are compared with `id` and `token` blanked because both are freshly minted; every
/// other field must agree exactly, including `is_active: true` (set by `PreSave`, not by the
/// body) and the `user_id` the **URL** named rather than the one the body did.
#[tokio::test]
async fn a_created_token_carries_a_fresh_secret_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let bot = TOKEN_BOT;
    let path = format!("/api/v4/users/{bot}/tokens");

    // `user_id` and `token` in the body are overwritten by the handler, `is_active` by `PreSave`.
    let body = format!(
        r#"{{"id":"zzzzzzzzzzzzzzzzzzzzzzzzzz","token":"mine","user_id":"{}","description":"mmrs-write created","is_active":false,"expires_at":0}}"#,
        logged_in_user_id()
    );
    let ((go_status, go), (rs_status, rs)) =
        post_both_raw(&client, &token, &path, body.as_bytes()).await;

    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{}", String::from_utf8_lossy(&rs));
    assert_eq!(
        comparable(&go, "go"),
        comparable(&rs, "rust"),
        "go={} rust={}",
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs)
    );

    let decoded = comparable(&rs, "rust");
    assert_eq!(decoded["user_id"], bot, "the URL's user, not the body's");
    assert_eq!(decoded["is_active"], true, "PreSave, not the body");
    assert_eq!(decoded["description"], "mmrs-write created");
    assert_eq!(decoded["expires_at"], 0);
    assert_eq!(
        decoded.as_object().expect("an object").len(),
        6,
        "six keys: {decoded}"
    );

    // `json.NewEncoder(w).Encode` — this route ends in a newline and the search route does not.
    assert!(rs.ends_with(b"\n"), "the encoder's newline");
    assert!(go.ends_with(b"\n"));

    // And the secret we were handed is really the one in the table.
    let minted: serde_json::Value = serde_json::from_slice(&rs).expect("json");
    let stored = row(minted["id"].as_str().expect("an id"))
        .await
        .expect("the row was written");
    assert_eq!(
        stored.token,
        minted["token"].as_str().expect("a secret"),
        "the response's secret is the stored one — this is the only time a client sees it"
    );

    sweep().await;
}

/// **The unreachable 400.** Revoke, disable and enable all do
/// `if tokenId == "" { c.SetInvalidParam("token_id") }` *without returning*, so the error they set
/// is overwritten by the 404 from looking up the empty id. A port that returned early would answer
/// 400 where Go answers 404 — which is why this test exists and why it covers a malformed body
/// too: `MapFromJSON` turns `[]`, `garbage` and `{}` into the same empty map.
#[tokio::test]
async fn an_empty_token_id_is_a_404_on_the_three_map_routes() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for route in ["revoke", "disable", "enable"] {
        let path = format!("/api/v4/users/tokens/{route}");
        for body in [&b"{}"[..], b"[]", b"garbage", b"", br#"{"token_id":""}"#] {
            let context = format!("{path} <- {}", String::from_utf8_lossy(body));
            let ((go_status, go), (rs_status, rs)) =
                post_both_raw(&client, &token, &path, body).await;
            assert_eq!(go_status, 404, "{context}: not the 400 it looks like");
            assert_eq!(rs_status, go_status, "{context}");
            let go_body = assert_error_bodies_match_except_known_gaps(&go, &rs, &context);
            assert_eq!(
                go_body["id"], "app.user_access_token.get_by_user.app_error",
                "{context}"
            );
        }

        // A real id that does not exist takes the same path, which is what makes the branch above
        // a *shape* rather than a coincidence of the empty string.
        let ((go_status, _), (rs_status, _)) = post_both_raw(
            &client,
            &token,
            &path,
            br#"{"token_id":"zzzzzzzzzzzzzzzzzzzzzzzzzz"}"#,
        )
        .await;
        assert_eq!(go_status, 404, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
    }

    sweep().await;
}

/// Rotate's identical-looking three lines **do** return, so its 400 is real. And the body decode
/// splits the parameter name three ways, which is the whole reason `decode_go_struct` exists:
/// `null` decodes to a zero struct and names `token_id`, an array fails to decode and names
/// `rotate_user_access_token`.
#[tokio::test]
async fn rotate_returns_the_four_hundred_its_siblings_do_not() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let path = "/api/v4/users/tokens/rotate";

    for (body, name) in [
        (&b"{}"[..], "token_id"),
        (b"null", "token_id"),
        (br#"{"token_id":""}"#, "token_id"),
        (b"[]", "rotate_user_access_token"),
        (b"", "rotate_user_access_token"),
        (b"garbage", "rotate_user_access_token"),
        (br#"{"expires_at":"soon"}"#, "rotate_user_access_token"),
    ] {
        let context = format!("{path} <- {}", String::from_utf8_lossy(body));
        let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, path, body).await;
        assert_eq!(go_status, 400, "{context}");
        assert_eq!(rs_status, go_status, "{context}");
        let go_body = assert_error_bodies_match_except_known_gaps(&go, &rs, &context);
        assert_eq!(
            go_body["id"], "api.context.invalid_body_param.app_error",
            "{context}"
        );
        // The parameter name is not decoration: it is the only thing distinguishing these three
        // failures, and Go's message interpolates it.
        assert!(
            go_body["message"]
                .as_str()
                .is_some_and(|m| m.contains(name)),
            "{context}: Go's message must name `{name}`, got {}",
            go_body["message"]
        );
    }

    sweep().await;
}

/// Disable deletes the sessions the token minted and leaves the row; enable puts it back and
/// touches no session. One planted fixture per server, so each write lands once.
#[tokio::test]
async fn disable_then_enable_moves_is_active_and_takes_the_session() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();

    let Some(go_token) = plant_token("disgo", me, true, 0).await else {
        return;
    };
    let rs_token = plant_token("disrs", me, true, 0)
        .await
        .expect("the second fixture");
    assert!(plant_session_for("disgo", me).await);
    assert!(plant_session_for("disrs", me).await);

    for (route, active) in [("disable", false), ("enable", true)] {
        let path = format!("/api/v4/users/tokens/{route}");
        let go = post_token_id(common::GO, &token, &path, &go_token).await;
        let rs = post_token_id(common::RUST, &token, &path, &rs_token).await;

        assert_eq!(go, (200, br#"{"status":"OK"}"#.to_vec()), "{path} on Go");
        assert_eq!(
            rs, go,
            "{path}: the bodies are `ReturnStatusOK`, no newline"
        );

        assert_eq!(
            row(&go_token).await.expect("Go's row survives").is_active,
            active,
            "{path}: Go"
        );
        assert_eq!(
            row(&rs_token).await.expect("our row survives").is_active,
            active,
            "{path}: us"
        );
    }

    // Disabling deleted both sessions; enabling did not put them back — Go has no statement that
    // could, and neither do we.
    assert!(!session_exists("disgo").await, "Go swept its session");
    assert!(!session_exists("disrs").await, "we swept ours");

    sweep().await;
}

/// Revoke takes the row **and** the session, in one transaction. A second revoke of the same id is
/// a 404 on both, which is what makes the first one's effect visible.
#[tokio::test]
async fn revoking_a_token_takes_its_session_with_it() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let path = "/api/v4/users/tokens/revoke";

    let Some(go_token) = plant_token("revgo", me, true, 0).await else {
        return;
    };
    let rs_token = plant_token("revrs", me, true, 0)
        .await
        .expect("the second fixture");
    assert!(plant_session_for("revgo", me).await);
    assert!(plant_session_for("revrs", me).await);
    assert!(plant_session_for("bystander", me).await);

    let go = post_token_id(common::GO, &token, path, &go_token).await;
    let rs = post_token_id(common::RUST, &token, path, &rs_token).await;
    assert_eq!(go, (200, br#"{"status":"OK"}"#.to_vec()));
    assert_eq!(rs, go);

    assert!(row(&go_token).await.is_none(), "Go deleted its row");
    assert!(row(&rs_token).await.is_none(), "we deleted ours");
    assert!(!session_exists("revgo").await, "and Go's session with it");
    assert!(!session_exists("revrs").await, "and ours");
    // **And nothing else.** The join is `o.Token = s.Token AND o.Id = ?`; losing either predicate
    // deletes every session on the installation, which is not a mutation this plan dares run — so
    // the bystander is asserted here instead. It belongs to no token at all.
    assert!(
        session_exists("bystander").await,
        "a session that no revoked token minted must survive"
    );

    // The row is gone, so the same call is now the family's 404.
    let ((go_status, _), (rs_status, _)) = post_both_raw(
        &client,
        &token,
        path,
        format!(r#"{{"token_id":"{go_token}"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);

    sweep().await;
}

/// Rotate replaces the secret and the expiry, returns the new secret, and refuses a **disabled**
/// token with the family's only `api.` id — a 400 among 403s.
#[tokio::test]
async fn rotate_replaces_the_secret_and_refuses_a_disabled_token() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let bot = TOKEN_BOT;
    let path = "/api/v4/users/tokens/rotate";

    // Created through the route so both rows are real tokens owned by the exempt bot.
    let create = format!("/api/v4/users/{bot}/tokens");
    let ((_, go_created), (_, rs_created)) = post_both_raw(
        &client,
        &token,
        &create,
        br#"{"description":"mmrs-write rotate"}"#,
    )
    .await;
    let go_id = serde_json::from_slice::<serde_json::Value>(&go_created).expect("json")["id"]
        .as_str()
        .expect("an id")
        .to_owned();
    let rs_id = serde_json::from_slice::<serde_json::Value>(&rs_created).expect("json")["id"]
        .as_str()
        .expect("an id")
        .to_owned();

    let before = row(&rs_id).await.expect("our token exists");
    assert_eq!(before.expires_at, 0, "created without an expiry");

    // A session minted by the **old** secret. The store's DELETE joins on that value, so it must
    // run before the UPDATE replaces it — reversing the two orphans this row: still valid, still
    // authenticating, and no longer reachable from the token that would revoke it.
    let orphan = plant_session_on_secret("rotrs", &before.token).await;
    assert!(orphan, "the session fixture is written");

    let expires_at = 1_988_600_000_000i64;
    let body = |id: &str| format!(r#"{{"token_id":"{id}","expires_at":{expires_at}}}"#);
    let go = post_one(common::GO, &token, path, &body(&go_id)).await;
    let rs = post_one(common::RUST, &token, path, &body(&rs_id)).await;

    assert_eq!(go.0, 200, "{}", String::from_utf8_lossy(&go.1));
    assert_eq!(rs.0, go.0, "{}", String::from_utf8_lossy(&rs.1));
    assert_eq!(
        comparable(&go.1, "go"),
        comparable(&rs.1, "rust"),
        "go={} rust={}",
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1)
    );
    assert!(rs.1.ends_with(b"\n"), "the encoder's newline, like create");

    let after = row(&rs_id).await.expect("our token survives a rotation");
    assert_ne!(after.token, before.token, "the secret really changed");
    assert!(
        !session_exists_with_secret(&before.token).await,
        "the session the old secret minted is gone, not orphaned"
    );
    assert_eq!(after.expires_at, expires_at, "and so did the expiry");
    let returned: serde_json::Value = serde_json::from_slice(&rs.1).expect("json");
    assert_eq!(
        returned["token"].as_str(),
        Some(after.token.as_str()),
        "the response carries the new secret, not the old one"
    );
    assert_eq!(returned["expires_at"], expires_at);

    // A disabled token cannot be rotated — enable it first. This is the only `api.` id in the
    // family and the only 400 among its refusals.
    let disable = "/api/v4/users/tokens/disable";
    assert_eq!(
        post_token_id(common::GO, &token, disable, &go_id).await.0,
        200
    );
    assert_eq!(
        post_token_id(common::RUST, &token, disable, &rs_id).await.0,
        200
    );

    let go = post_one(common::GO, &token, path, &body(&go_id)).await;
    let rs = post_one(common::RUST, &token, path, &body(&rs_id)).await;
    assert_eq!(go.0, 400);
    assert_eq!(rs.0, go.0);
    let go_body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, "rotate a disabled");
    assert_eq!(
        go_body["id"],
        "api.user.rotate_user_access_token.disabled_token.app_error"
    );

    sweep().await;
}

/// **The search term is not a pattern.** `sanitizeSearchTerm` escapes `%` and `_` and nothing
/// wraps the term, so all three `LIKE`s are equalities: an id matches itself, a username matches
/// itself, a prefix matches nothing and `%` matches nothing at all.
///
/// Compared as **sets** — the query has no `ORDER BY`, so the row order is Postgres's and is not a
/// parity property.
#[tokio::test]
async fn the_search_term_matches_exactly_and_never_wildcards() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let path = "/api/v4/users/tokens/search";

    sweep().await;
    let Some(first) = plant_token("srcha", me, true, 0).await else {
        return;
    };
    let second = plant_token("srchb", me, false, 1788600000000)
        .await
        .expect("the second fixture");
    let owner_username = common::username_of(&client, &token, me).await;

    let ids = |body: &[u8]| -> std::collections::BTreeSet<String> {
        serde_json::from_slice::<Vec<serde_json::Value>>(body)
            .expect("a JSON array")
            .into_iter()
            .map(|t| t["id"].as_str().expect("an id").to_owned())
            .collect()
    };

    for (term, expected) in [
        // The token's own id, exactly.
        (
            first.clone(),
            std::collections::BTreeSet::from([first.clone()]),
        ),
        // The owner's id matches **both** of their tokens.
        (
            me.to_owned(),
            std::collections::BTreeSet::from([first.clone(), second.clone()]),
        ),
        // And so does the owner's **username**, through the `INNER JOIN Users`. This is the third
        // `LIKE` and the only one a token id or user id cannot also satisfy.
        (
            owner_username.clone(),
            std::collections::BTreeSet::from([first.clone(), second.clone()]),
        ),
        // A prefix of a matching id finds nothing: there is no trailing `%`.
        (first[..10].to_owned(), std::collections::BTreeSet::new()),
        // And a wildcard is escaped into a literal.
        ("%".to_owned(), std::collections::BTreeSet::new()),
        ("_".to_owned(), std::collections::BTreeSet::new()),
    ] {
        let context = format!("{path} <- {term}");
        let body = serde_json::json!({ "term": term }).to_string();
        let ((go_status, go), (rs_status, rs)) =
            post_both_raw(&client, &token, path, body.as_bytes()).await;
        assert_eq!(go_status, 200, "{context}");
        assert_eq!(rs_status, go_status, "{context}");
        assert_eq!(ids(&go), expected, "{context}: Go's answer is the oracle");
        assert_eq!(ids(&rs), ids(&go), "{context}");
        // `json.Marshal` + `w.Write`, unlike create and rotate.
        assert!(!rs.ends_with(b"\n"), "{context}: no encoder newline");
    }

    // The secret is blanked, exactly as on the four reads.
    let body = serde_json::json!({ "term": me }).to_string();
    let ((_, go), (_, rs)) = post_both_raw(&client, &token, path, body.as_bytes()).await;
    for (who, raw) in [("go", &go), ("rust", &rs)] {
        let text = String::from_utf8_lossy(raw);
        assert!(
            !text.contains(&secret_for("srcha")),
            "{who} leaked the secret: {text}"
        );
        assert!(!text.contains("\"token\""), "{who}: no token key: {text}");
    }

    sweep().await;
}

/// The destructive twin of `non_compliant/count` disagrees with it about a disabled policy: the
/// count answers `{"count":0}`, this answers **400**. On a stock server that 400 is the only
/// answer the route gives, which makes it worth pinning.
#[tokio::test]
async fn revoking_non_compliant_tokens_is_refused_while_no_policy_is_set() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let path = "/api/v4/users/tokens/non_compliant/revoke";

    // A never-expiring token is exactly what a policy would call non-compliant, so its survival is
    // the assertion: the refusal happens before anything is deleted.
    let Some(planted) = plant_token("ncomp", me, true, 0).await else {
        return;
    };

    let ((go_status, go), (rs_status, rs)) = post_both_raw(&client, &token, path, b"").await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go_body = assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    assert_eq!(
        go_body["id"],
        "app.user_access_token.revoke_non_compliant.no_policy.app_error"
    );
    assert!(
        row(&planted).await.is_some(),
        "nothing was revoked on either server"
    );

    sweep().await;
}

/// The two `manage_system` routes refuse a plain user, and the four-permission table in
/// `mm_api::tokens` says they refuse with `manage_system` itself rather than `edit_other_users` —
/// there is no second, per-user gate on either.
#[tokio::test]
async fn search_and_the_non_compliant_sweep_need_manage_system() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::a_team_and_channel_the_user_is_in(&client, &admin)
        .await
        .0;
    let plain = common::create_plain_user(&client, &admin, &team, "patw").await;

    for (path, body) in [
        ("/api/v4/users/tokens/search", &br#"{"term":"x"}"#[..]),
        ("/api/v4/users/tokens/non_compliant/revoke", b""),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            post_both_raw(&client, &plain.token, path, body).await;
        assert_eq!(go_status, 403, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
        let go_body = assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        assert_eq!(go_body["id"], "api.context.permissions.app_error", "{path}");
        // **`detailed_error` is empty, and that is the interesting part.** `MakePermissionError`
        // fills it with `userId=…, permission=manage_system`, and `handleContextError` then wipes
        // it because `ServiceSettings.EnableDeveloper` is off (web/handlers.go:436). So the string
        // that names which permission was wanted never reaches a client on a stock server — on
        // either side. The helper above already compares the field; this pins why it is blank.
        assert_eq!(go_body["detailed_error"], "", "{path}");
    }

    common::delete_plain_user(&client, &admin, &plain.id).await;
    sweep().await;
}

/// **Enable is gated on `create_user_access_token`, its mirror `disable` on
/// `revoke_user_access_token`.** An admin holds both, so nothing above can tell the two apart;
/// this plants a role holding exactly one and checks each direction.
///
/// The property is not cosmetic: if enable took the revoke permission, a caller whose only power
/// is to *withdraw* credentials could re-arm every one they had disabled.
///
/// The token belongs to the caller, so `SessionHasPermissionToUserOrBot` passes on the owner arm
/// and the gating permission is the only variable.
#[tokio::test]
async fn enable_and_disable_are_gated_on_opposite_permissions() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = common::a_team_and_channel_the_user_is_in(&client, &admin)
        .await
        .0;

    for (tag, permission, allowed, refused) in [
        ("patrev", "revoke_user_access_token", "disable", "enable"),
        ("patcre", "create_user_access_token", "enable", "disable"),
    ] {
        let Some(role) = common::plant_role(tag, permission).await else {
            return; // no DATABASE_URL
        };
        let user = common::create_plain_user(&client, &admin, &team, tag).await;
        common::set_user_roles(&user.id, &format!("system_user {role}")).await;
        let token = common::login_plain_user(&client, tag).await;

        let go_token = plant_token(&format!("{tag}g"), &user.id, true, 0)
            .await
            .expect("Go's fixture");
        let rs_token = plant_token(&format!("{tag}r"), &user.id, true, 0)
            .await
            .expect("our fixture");

        let path = format!("/api/v4/users/tokens/{allowed}");
        let go = post_token_id(common::GO, &token, &path, &go_token).await;
        let rs = post_token_id(common::RUST, &token, &path, &rs_token).await;
        assert_eq!(go.0, 200, "{permission} may {allowed}: {:?}", go.1);
        assert_eq!(rs.0, go.0, "{path}: {}", String::from_utf8_lossy(&rs.1));

        let path = format!("/api/v4/users/tokens/{refused}");
        let go = post_token_id(common::GO, &token, &path, &go_token).await;
        let rs = post_token_id(common::RUST, &token, &path, &rs_token).await;
        assert_eq!(go.0, 403, "{permission} may not {refused}");
        assert_eq!(rs.0, go.0, "{path}: {}", String::from_utf8_lossy(&rs.1));
        let go_body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, &path);
        assert_eq!(go_body["id"], "api.context.permissions.app_error", "{path}");

        common::delete_plain_user(&client, &admin, &user.id).await;
    }

    sweep().await;
}
