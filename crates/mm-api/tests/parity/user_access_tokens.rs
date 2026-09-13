//! Cross-server parity for the four personal-access-token reads. The **writes** are next door in
//! [`crate::parity::token_writes`], which shares this module's [`TOKENS`] lock and its fixture
//! helpers — both suites plant into one table that `GET /users/tokens` lists whole, so there is no
//! per-test scope for either of them to hide behind.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity user_access_tokens
//! ```
//!
//! # The rows are planted, because the route that creates them is closed
//!
//! `POST /users/{user_id}/tokens` needs `ServiceSettings.EnableUserAccessTokens`, which is off on
//! a stock server, so `UserAccessTokens` is **empty** on this deployment: every one of the four
//! routes answers `[]` or `{"count":0}` and a suite built on that proves only that both servers
//! can serialise an empty list. The rows below are written directly, the same way `plant_role` and
//! `plant_bot` are, so the sanitisation and the permission cascade are exercised against real
//! data.
//!
//! # What is worth checking
//!
//! **The secret never reaches a client** — asserted against the live response, not a constructed
//! one — and the permission rules, which differ on all four routes and are the only thing standing
//! between a `read_user_access_token` holder and someone else's credentials.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, RUST, assert_error_bodies_match_except_known_gaps, client,
    create_plain_user, create_team, fetch_both_raw, go_minted_token, logged_in_user_id,
    stack_enabled,
};

const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// **Every test here holds this**, because they all plant into and sweep the same table: one
/// test's `unplant_tokens` would otherwise delete another's fixture mid-assertion, and
/// `GET /users/tokens` lists the whole installation so there is no per-test scope to hide behind.
pub(crate) static TOKENS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The planted secret for a tag. **Distinct per tag**: `UserAccessTokens.Token` is uniquely
/// indexed, so a shared constant makes the second plant a constraint violation.
///
/// The secret is what this suite is about, so it is recognisable: a body carrying it anywhere is a
/// leak, and searching for the literal is a stronger assertion than checking a key's absence.
pub(crate) fn secret_for(tag: &str) -> String {
    format!("mmrssecret{tag:0>16}")
}

/// A `UserAccessTokens` row, written directly. Returns the token's id.
pub(crate) async fn plant_token(
    tag: &str,
    user_id: &str,
    is_active: bool,
    expires_at: i64,
) -> Option<String> {
    let pool = common::fixture_pool().await?;
    let id = format!("mmrstok{tag:0>19}");
    sqlx::query(
        "INSERT INTO useraccesstokens (id, token, userid, description, isactive, expiresat,
                                       lastnotifiedat)
         VALUES ($1, $2, $3, $4, $5, $6, NULL)
         ON CONFLICT (id) DO UPDATE SET userid = EXCLUDED.userid,
                                        isactive = EXCLUDED.isactive,
                                        expiresat = EXCLUDED.expiresat",
    )
    .bind(&id)
    .bind(secret_for(tag))
    .bind(user_id)
    .bind(format!("planted by the parity suite ({tag})"))
    .bind(is_active)
    .bind(expires_at)
    .execute(&pool)
    .await
    .expect("the token row is written");
    Some(id)
}

pub(crate) async fn unplant_tokens() {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    sqlx::query("DELETE FROM useraccesstokens WHERE id LIKE 'mmrstok%'")
        .execute(&pool)
        .await
        .expect("the planted tokens are removed");
}

async fn served_by(client: &reqwest::Client, token: &str, path: &str) -> Option<String> {
    client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer")
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// **The security property**, asserted against the live bodies of all four routes: the planted
/// secret appears in none of them.
#[tokio::test]
async fn no_route_leaks_the_secret() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();

    let Some(planted) = plant_token("leak", me, true, 0).await else {
        return; // no DATABASE_URL
    };
    let secret = secret_for("leak");

    for path in [
        "/api/v4/users/tokens".to_owned(),
        format!("/api/v4/users/{me}/tokens"),
        format!("/api/v4/users/tokens/{planted}"),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, 200, "{path}");
        assert_eq!(rs_status, go_status, "{path}");

        let go_text = String::from_utf8_lossy(&go);
        let rs_text = String::from_utf8_lossy(&rs);
        assert!(
            !go_text.contains(&secret),
            "{path}: Go leaked the secret, which would make this comparison meaningless"
        );
        assert!(!rs_text.contains(&secret), "{path}: we leaked the secret");
        assert!(
            rs_text.contains(&planted),
            "{path}: the planted token is in the answer, so this is not vacuous: {rs_text}"
        );
        assert_eq!(
            served_by(&client, &token, &path).await.as_deref(),
            Some("rust"),
            "{path}"
        );
    }

    unplant_tokens().await;
}

/// The bodies, byte for byte, and the one route whose body ends in a newline.
#[tokio::test]
async fn the_bodies_match_and_only_one_has_a_newline() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();

    let Some(planted) = plant_token("bytes", me, true, 1788600000000).await else {
        return;
    };

    let single = format!("/api/v4/users/tokens/{planted}");
    let ((_, go), (_, rs)) = fetch_both_raw(&client, &token, &single).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{single}"
    );
    assert!(
        rs.ends_with(b"\n"),
        "`json.NewEncoder(w).Encode` writes a newline"
    );

    // Five keys, and `token` is not among them.
    let decoded: serde_json::Value = serde_json::from_slice(&rs).expect("json");
    let object = decoded.as_object().expect("an object");
    assert_eq!(object.len(), 5, "{decoded}");
    for key in ["id", "user_id", "description", "is_active", "expires_at"] {
        assert!(object.contains_key(key), "{key}: {decoded}");
    }
    assert_eq!(decoded["expires_at"], 1788600000000i64);

    // The three `json.Marshal` routes carry no newline.
    for path in [
        "/api/v4/users/tokens".to_owned(),
        format!("/api/v4/users/{me}/tokens"),
        "/api/v4/users/tokens/non_compliant/count".to_owned(),
    ] {
        let ((_, go), (_, rs)) = fetch_both_raw(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
        assert!(
            !rs.ends_with(b"\n"),
            "{path}: `json.Marshal` writes no newline"
        );
    }

    unplant_tokens().await;
}

/// An empty list is `[]`, never `null` — the store's `tokens := []*model.UserAccessToken{}`.
#[tokio::test]
async fn an_empty_page_is_an_empty_array() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    unplant_tokens().await;

    for path in [
        "/api/v4/users/tokens",
        "/api/v4/users/tokens?page=99",
        // A user who has never minted one.
        "/api/v4/users/zzzzzzzzzzzzzzzzzzzzzzzzzz/tokens",
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, path).await;
        assert_eq!(go_status, 200, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
        assert_eq!(go, b"[]", "{path}: Go's empty page");
        assert_eq!(rs, go, "{path}");
    }
}

/// **The count reads nothing on a stock server.** `MaximumPersonalAccessTokenLifetimeDays` is 0,
/// so the policy is off and the answer is `{"count":0}` even with a never-expiring token planted —
/// which is what makes it a fixture worth planting.
#[tokio::test]
async fn the_non_compliant_count_is_zero_while_the_policy_is_off() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();

    // `expires_at = 0` means "never expires", which is precisely what the policy would count.
    let Some(_planted) = plant_token("count", me, true, 0).await else {
        return;
    };

    let path = "/api/v4/users/tokens/non_compliant/count";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        r#"{"count":0}"#,
        "the policy is off, so the never-expiring token is not counted"
    );
    assert_eq!(rs, go);
    assert_eq!(
        served_by(&client, &token, path).await.as_deref(),
        Some("rust")
    );

    unplant_tokens().await;
}

/// The four permission rules, which are four different rules.
#[tokio::test]
async fn each_route_has_its_own_permission_rule() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "tokens").await;
    let user = create_plain_user(&client, &admin, &team, "tokens").await;
    let me = logged_in_user_id();

    let Some(planted) = plant_token("perm", me, true, 0).await else {
        return;
    };

    // A plain user is refused all four, and the id each refusal names differs.
    for (path, want) in [
        ("/api/v4/users/tokens".to_owned(), "manage_system"),
        (
            "/api/v4/users/tokens/non_compliant/count".to_owned(),
            "manage_system",
        ),
        (
            format!("/api/v4/users/tokens/{planted}"),
            "read_user_access_token",
        ),
        (
            format!("/api/v4/users/{me}/tokens"),
            "read_user_access_token",
        ),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &user.token, &path).await;
        assert_eq!(go_status, 403, "{path}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status, "{path}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(body["id"], "api.context.permissions.app_error", "{path}");
        // The permission itself is in `detailed_error`, which `WipeDetailed` removes — so the
        // name is recorded here rather than asserted, and the *status* is what a client sees.
        let _ = want;
        assert_eq!(
            served_by(&client, &user.token, &path).await.as_deref(),
            Some("rust"),
            "{path}"
        );
    }

    // A plain user reading **their own** tokens is still refused: `read_user_access_token` is a
    // system-scoped permission no stock role grants, so "my own tokens" is not self-service.
    let own = format!("/api/v4/users/{}/tokens", user.id);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &user.token, &own).await;
    assert_eq!(go_status, 403, "{own}");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go, &rs, &own);

    unplant_tokens().await;
    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// **The single-token route fetches before it checks the owner**, so it answers 404 for an id that
/// does not exist and 403 for one that does — to the same caller. Both halves are asserted,
/// because only the pair shows the ordering.
#[tokio::test]
async fn the_single_token_route_checks_the_owner_after_the_fetch() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "tokord").await;
    let user = create_plain_user(&client, &admin, &team, "tokord").await;

    let Some(role) = common::plant_role("tokord", "read_user_access_token").await else {
        return;
    };
    common::set_user_roles(&user.id, &format!("system_user {role}")).await;
    let reader = common::login_plain_user(&client, "tokord").await;

    let Some(theirs) = plant_token("ord", logged_in_user_id(), true, 0).await else {
        return;
    };
    let Some(mine) = plant_token("ownd", &user.id, true, 0).await else {
        return;
    };

    // An id that names nothing: **404**, from the fetch.
    let missing = format!("/api/v4/users/tokens/{NOWHERE}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader, &missing).await;
    assert_eq!(go_status, 404, "{missing}");
    assert_eq!(rs_status, go_status);
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &missing);
    assert_eq!(body["id"], "app.user_access_token.get_by_user.app_error");

    // An id that names someone else's token: **403**, from the owner check after the fetch.
    let other = format!("/api/v4/users/tokens/{theirs}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader, &other).await;
    assert_eq!(
        go_status,
        403,
        "{other}: the owner check runs after the fetch: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status);
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &other);
    assert_eq!(body["id"], "api.context.permissions.app_error");

    // And their own token, which the same caller may read — so the 403 above is about ownership
    // and not about the permission.
    let own = format!("/api/v4/users/tokens/{mine}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader, &own).await;
    assert_eq!(go_status, 200, "{own}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{own}"
    );
    assert!(!String::from_utf8_lossy(&rs).contains(&secret_for("ownd")));

    // **`manage_system` is not `read_user_access_token`.** This caller holds the second and not
    // the first, so the two installation-wide routes are refused while the two token-scoped ones
    // admit — which is the only way to tell the two permissions apart, since every stock role that
    // grants either grants both.
    for path in [
        "/api/v4/users/tokens",
        "/api/v4/users/tokens/non_compliant/count",
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader, path).await;
        assert_eq!(
            go_status,
            403,
            "{path}: read_user_access_token does not open the installation-wide routes: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{path}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    }

    // The list form of the same rule: their own user id is admitted, the admin's is not.
    let own_list = format!("/api/v4/users/{}/tokens", user.id);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader, &own_list).await;
    assert_eq!(go_status, 200, "{own_list}");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{own_list}"
    );

    let their_list = format!("/api/v4/users/{}/tokens", logged_in_user_id());
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader, &their_list).await;
    assert_eq!(go_status, 403, "{their_list}");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go, &rs, &their_list);

    unplant_tokens().await;
    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// `RequireTokenId` and `RequireUserId`, and the `me` alias on the list route.
#[tokio::test]
async fn the_id_checks_agree_and_me_resolves() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (path, id) in [
        (
            "/api/v4/users/tokens/short",
            "api.context.invalid_url_param.app_error",
        ),
        (
            "/api/v4/users/short/tokens",
            "api.context.invalid_url_param.app_error",
        ),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        assert_eq!(body["id"], id, "{path}");
    }

    // `me` resolves before validation, so the alias is a 200 rather than a 400.
    let me_path = "/api/v4/users/me/tokens";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, me_path).await;
    assert_eq!(go_status, 200, "{me_path}");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{me_path}"
    );
    assert_eq!(
        served_by(&client, &token, me_path).await.as_deref(),
        Some("rust")
    );
}

/// **A refusal on a bot must not become a grant on a user.**
///
/// `SessionHasPermissionToUserOrBot` resolves "or bot" by trying the bot path and reading its
/// failure — and only `store.sql_bot.get.missing.app_error` **from `SqlBotStore.Get`** falls
/// through to the ordinary user check. A caller refused on an *existing* bot gets the same id with
/// `where` = `permissions`, and that one must not fall through: it would hand a caller holding
/// `edit_other_users` the access tokens of every bot on the installation.
///
/// The fixture is the narrow one that can tell them apart — `read_others_bots` without
/// `manage_others_bots`, so the bot path refuses with a *permission* error rather than the
/// existence-hiding 404, plus `edit_other_users` so the user path would say yes.
#[tokio::test]
async fn a_refusal_on_a_bot_does_not_fall_through_to_the_user_check() {
    if !stack_enabled() {
        return;
    }
    let _tokens = TOKENS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    // **This test sweeps the whole `mmrsbot%` prefix on the way out**, so it has to hold the lock
    // every other bot-planting suite holds. It did not, and a full-workspace run deleted a
    // `parity::user_convert` fixture between that suite's conversion and its read-back — a
    // failure naming a route this file never touches. The sweep is `unplant_bots`, not
    // `unplant_bot`, which is what makes the lock necessary rather than merely tidy.
    let _bots = common::BOT_FIXTURES.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "tokbot").await;
    let user = create_plain_user(&client, &admin, &team, "tokbot").await;

    let Some(role) = common::plant_role(
        "tokbot",
        "read_user_access_token edit_other_users read_others_bots",
    )
    .await
    else {
        return;
    };
    common::set_user_roles(&user.id, &format!("system_user {role}")).await;
    let reader = common::login_plain_user(&client, "tokbot").await;

    // A bot owned by the admin, and a token owned by that bot.
    let Some(bot) = common::plant_bot("tokbot", logged_in_user_id(), 0).await else {
        return;
    };
    let Some(bot_token) = plant_token("botok", &bot, true, 0).await else {
        return;
    };

    // The bot's token list: refused, because the bot path refuses and does **not** fall through.
    let list = format!("/api/v4/users/{bot}/tokens");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader, &list).await;
    assert_eq!(
        go_status,
        403,
        "{list}: a permission refusal on the bot is final: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{list}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, &list);

    // And the single-token form, which checks the same thing against the token's owner.
    let single = format!("/api/v4/users/tokens/{bot_token}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader, &single).await;
    assert_eq!(go_status, 403, "{single}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{single}");
    assert_error_bodies_match_except_known_gaps(&go, &rs, &single);

    // The contrast that makes the assertion mean something: the *same* caller, with the *same*
    // permissions, reading an ordinary user's tokens is admitted — `edit_other_users` carries the
    // user path. So the 403s above are the bot branch refusing, not the route refusing everyone.
    let ordinary = format!("/api/v4/users/{}/tokens", user.id);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &reader, &ordinary).await;
    assert_eq!(
        go_status,
        200,
        "{ordinary}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, go_status, "{ordinary}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{ordinary}"
    );

    unplant_tokens().await;
    common::unplant_bots().await;
    common::delete_plain_user(&client, &admin, &user.id).await;
}
