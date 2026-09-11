//! Cross-server parity for the five bot writes: `createBot`, `patchBot`, `disableBot`,
//! `enableBot` and `assignBot`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity bot_writes
//! ```
//!
//! # The create route is refused on this deployment, and that is the test
//!
//! `ServiceSettings.EnableBotAccountCreation` defaults to `false` and the stack leaves it there
//! (`scripts/stack.sh` writes its seeded bots straight to the tables for that reason). So every
//! `POST /bots` here is a 403, and what this suite can compare is **which** 403 — the permission
//! gate fires before the feature gate, and the body gate before both. The success path is
//! exercised by `mm_store`'s database tests instead; see [D-280].
//!
//! # Reads back through the server that wrote
//!
//! Both servers share one database, so a single object cannot be written by both. Every mutating
//! test plants **two** bots, drives one per server, and compares the answers with the values that
//! cannot match — the id, the username, the timestamps — normalised away. [D-190]'s rule.
//!
//! # The `Bots` row is only half of what these routes write
//!
//! A rename rewrites `Users.Email`; a disable sets `Users.DeleteAt`. Neither is in a `model.Bot`,
//! so the HTTP comparison alone would pass on a port that updated one table and not the other.
//! `common::bot_and_user_rows` is what closes that.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, client, create_plain_user, create_team, go_minted_token,
    stack_enabled,
};

/// Send a request to one server and read `(status, body)` back, asserting that a Rust answer was
/// actually served by Rust rather than forwarded.
async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    base: &str,
    token: &str,
    path: &str,
    body: Option<&[u8]>,
) -> (u16, Vec<u8>) {
    let mut request = http
        .request(method, format!("{base}{path}"))
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

/// The same request to both servers, for a route that changes nothing when it refuses.
async fn both(
    http: &reqwest::Client,
    method: reqwest::Method,
    token: &str,
    path: &str,
    body: Option<&[u8]>,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let go = send(http, method.clone(), GO, token, path, body).await;
    let rs = send(http, method, RUST, token, path, body).await;
    (go, rs)
}

/// Everything about a `model.Bot` that two independently planted bots can be expected to share.
///
/// `user_id` and `username` are the planting tag; `update_at` and `delete_at` are clocks. Each
/// becomes a **presence** assertion rather than being dropped, so a route that stopped setting
/// `delete_at` at all still fails.
fn normalise(raw: &[u8]) -> serde_json::Value {
    let mut value: serde_json::Value =
        serde_json::from_slice(raw).unwrap_or_else(|e| panic!("not a bot: {e}: {raw:?}"));
    let object = value.as_object_mut().expect("an object");
    for key in ["user_id", "username"] {
        let present = object
            .get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        object.insert(key.to_owned(), serde_json::json!(present));
    }
    for key in ["update_at", "delete_at", "create_at"] {
        let nonzero = object.get(key).and_then(|v| v.as_i64()).unwrap_or(0) > 0;
        object.insert(key.to_owned(), serde_json::json!(nonzero));
    }
    value
}

/// Plant one bot per server, owned by `owner_id`, and return `(go_bot_id, rust_bot_id)`.
async fn two_bots(tag: &str, owner_id: &str) -> Option<(String, String)> {
    let go = common::plant_bot(&format!("{tag}g"), owner_id, 0).await?;
    let rs = common::plant_bot(&format!("{tag}r"), owner_id, 0).await?;
    // `plant_bot` derives the description and display name from the tag, and the tags have to
    // differ because the ids do. Overwriting both with one value is what leaves `normalise` with
    // nothing to erase but the id, the username and the clocks — and it also resets a bot a
    // previous crashed run left behind, since `plant_bot`'s upsert does not touch the text.
    for bot in [&go, &rs] {
        common::set_bot_fixture_text(bot, "a bot the write suite planted", "Write Suite").await;
    }
    Some((go, rs))
}

/// **The `Name` parameter is not on the wire.** `model.AppError`'s parameter map carries
/// `json:"-"`, so a 400 naming `bot_user_id` and one naming `user_id` are byte-identical apart
/// from Go's translated `message` — which our server does not translate (D-092). Asserting it on
/// *Go's* body is therefore the strongest available claim: it pins what the route is supposed to
/// name, and the cross-server comparison pins everything a client can actually see.
fn go_message_names(go_body: &[u8], parameter: &str) {
    let go: serde_json::Value = serde_json::from_slice(go_body).expect("an error");
    let message = go["message"].as_str().unwrap_or_default();
    assert!(
        message.contains(parameter),
        "Go's message should name `{parameter}`: {message}"
    );
}

/// **Three gates, and the order between them is the whole test.**
///
/// The body is decoded before the permission is checked, and the permission before the feature
/// flag — so a plain user sending nonsense gets a 400, a plain user sending a valid bot gets the
/// permission 403, and only an admin ever learns that bot creation is disabled. A port that
/// checked the flag first would tell every caller the feature is off, which is a different answer
/// to three different callers.
#[tokio::test]
async fn the_create_gates_fire_in_gos_order() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "botw").await;
    let user = create_plain_user(&http, &admin, &team, "botw").await;

    let valid = br#"{"username":"mmrsbotnever","display_name":"Never","description":"never"}"#;

    // The admin holds `create_bot`, so it reaches the feature flag — and the flag is off.
    let ((go_status, go), (rs_status, rs)) = both(
        &http,
        reqwest::Method::POST,
        &admin,
        "/api/v4/bots",
        Some(valid),
    )
    .await;
    assert_eq!(rs_status, go_status, "POST /bots as the admin");
    assert_eq!(go_status, 403);
    let parsed =
        common::assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/bots (admin)");
    assert_eq!(
        parsed["id"], "api.bot.create_disabled",
        "EnableBotAccountCreation is false on this deployment; if this changed, \
         scripts/stack.sh's seeded bots changed too"
    );

    // A plain user never gets that far: no `create_bot`.
    let ((go_status, go), (rs_status, rs)) = both(
        &http,
        reqwest::Method::POST,
        &user.token,
        "/api/v4/bots",
        Some(valid),
    )
    .await;
    assert_eq!(rs_status, go_status, "POST /bots as a plain user");
    assert_eq!(go_status, 403);
    let parsed =
        common::assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/bots (plain)");
    assert_eq!(
        parsed["id"], "api.context.permissions.app_error",
        "the permission gate precedes the feature gate"
    );

    // And the body is decoded before either, so `null` is a 400 even for a caller with no rights.
    // `null` rather than garbage on purpose: Go decodes it *successfully* into a nil pointer and
    // catches it with `botPatch == nil`, which is a branch a malformed body never reaches.
    for body in [&b"null"[..], &b"{"[..], &b""[..]] {
        let ((go_status, go), (rs_status, rs)) = both(
            &http,
            reqwest::Method::POST,
            &user.token,
            "/api/v4/bots",
            Some(body),
        )
        .await;
        assert_eq!(rs_status, go_status, "POST /bots with {body:?}");
        assert_eq!(go_status, 400, "{body:?}");
        let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, "/api/v4/bots");
        assert_eq!(parsed["id"], "api.context.invalid_body_param.app_error");
        go_message_names(&go, "bot");
    }

    // Six refused creates, and not a row between them. `remove_users_named` deletes what it
    // counts, so a *failure* here also cleans up after itself — see its doc comment for the
    // mutation batch that needed that.
    assert_eq!(
        common::remove_users_named("mmrsbotnever").await,
        Some(0),
        "a refused create must write nothing"
    );

    common::delete_plain_user(&http, &admin, &user.id).await;
}

/// **A patch that changes nothing writes nothing** — same body back, same `update_at`, and both
/// rows untouched.
///
/// Safe to compare byte for byte on **one** bot precisely because neither server writes: this is
/// the only test in the file that can do that.
#[tokio::test]
async fn a_patch_that_changes_nothing_is_a_read() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    let Some(bot) = common::plant_bot("noop", me, 0).await else {
        return; // no DATABASE_URL
    };
    let before = common::bot_and_user_rows(&bot).await.expect("the rows");
    let path = format!("/api/v4/bots/{bot}");

    // `WouldPatch` compares each present field against the current value, so a patch naming the
    // values the bot already holds is a no-op — including the `null` fields, which mean
    // "unmentioned" and never "clear".
    let body = format!(
        r#"{{"username":"{}","display_name":"{}","description":"{}"}}"#,
        before["username"].as_str().expect("a username"),
        before["firstname"].as_str().expect("a display name"),
        before["description"].as_str().expect("a description"),
    );
    let ((go_status, go), (rs_status, rs)) = both(
        &http,
        reqwest::Method::PUT,
        &admin,
        &path,
        Some(body.as_bytes()),
    )
    .await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "a no-op patch answers identically on both servers"
    );
    assert!(rs.ends_with(b"\n"), "the encoder's newline");

    let after = common::bot_and_user_rows(&bot).await.expect("the rows");
    assert_eq!(
        before, after,
        "neither server may write a row for a patch that changes nothing"
    );

    // An all-`null` patch is the same answer through a different branch: `WouldPatch` returns
    // false for every absent field.
    let ((go_status, go), (rs_status, rs)) = both(
        &http,
        reqwest::Method::PUT,
        &admin,
        &path,
        Some(br#"{"username":null,"display_name":null,"description":null}"#),
    )
    .await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "an all-null patch is a read too"
    );
    assert_eq!(
        common::bot_and_user_rows(&bot).await.expect("the rows"),
        after
    );

    common::unplant_bot(&bot).await;
}

/// A real patch, on one bot per server: the answer, and the two rows behind it.
///
/// **The email is the assertion worth the file.** `UserFromBot` regenerates it from the username,
/// so renaming a bot silently rewrites `Users.Email` to `<username>@localhost` — a write no
/// `model.Bot` shows and no HTTP comparison can see.
#[tokio::test]
async fn a_patch_renames_the_user_row_and_rewrites_its_email() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    let Some((go_bot, rs_bot)) = two_bots("patch", me).await else {
        return;
    };

    // Distinct usernames — the column is unique — and identical display names and descriptions,
    // so the two answers differ only in what `normalise` erases.
    let patch_for = |name: &str| {
        format!(r#"{{"username":"{name}","display_name":"Patched Name","description":"patched"}}"#)
    };
    let (go_status, go) = send(
        &http,
        reqwest::Method::PUT,
        GO,
        &admin,
        &format!("/api/v4/bots/{go_bot}"),
        Some(patch_for("mmrsbotpatchedgo").as_bytes()),
    )
    .await;
    let (rs_status, rs) = send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &admin,
        &format!("/api/v4/bots/{rs_bot}"),
        Some(patch_for("mmrsbotpatchedrs").as_bytes()),
    )
    .await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go));
    assert_eq!(normalise(&go), normalise(&rs), "the patched bots");
    assert_eq!(
        normalise(&rs)["display_name"],
        "Patched Name",
        "the patch applied"
    );
    assert!(rs.ends_with(b"\n"));

    let go_rows = common::bot_and_user_rows(&go_bot).await.expect("rows");
    let rs_rows = common::bot_and_user_rows(&rs_bot).await.expect("rows");

    for (rows, name) in [
        (&go_rows, "mmrsbotpatchedgo"),
        (&rs_rows, "mmrsbotpatchedrs"),
    ] {
        assert_eq!(rows["username"], name, "the Users row was renamed");
        assert_eq!(
            rows["email"],
            format!("{name}@localhost"),
            "UserFromBot regenerates the address from the username; the planted \
             `@mmrs.invalid` is gone"
        );
        assert_eq!(
            rows["firstname"], "Patched Name",
            "a bot's display name is Users.FirstName"
        );
        assert_eq!(rows["description"], "patched");
        assert_eq!(rows["user_delete_at"], 0);
        assert_eq!(rows["bot_delete_at"], 0);
        assert!(
            rows["bot_update_at"].as_i64().unwrap_or(0) > 1788600000000,
            "the Bots row was written"
        );
        assert!(
            rows["user_update_at"].as_i64().unwrap_or(0) > 1788600000000,
            "and so was the Users row"
        );
    }

    common::unplant_bot(&go_bot).await;
    common::unplant_bot(&rs_bot).await;
}

/// Disable, disable again, enable — and the asymmetry between the two rows.
///
/// **The `Bots` row is idempotent and the `Users` row is not.** A second disable leaves
/// `Bots.UpdateAt` exactly where the first put it (Go's `changed` guard) while still bumping
/// `Users.UpdateAt`, because `UpdateActive` runs unconditionally above the guard. Both servers do
/// it, and a port with the guard in the wrong place passes every status-code test.
#[tokio::test]
async fn disable_and_enable_flip_both_rows_and_only_one_is_idempotent() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();

    let Some((go_bot, rs_bot)) = two_bots("act", me).await else {
        return;
    };

    let act = async |base: &str, bot: &str, what: &str| -> (u16, Vec<u8>) {
        send(
            &http,
            reqwest::Method::POST,
            base,
            &admin,
            &format!("/api/v4/bots/{bot}/{what}"),
            None,
        )
        .await
    };

    // --- disable ---------------------------------------------------------------------------
    let (go_status, go) = act(GO, &go_bot, "disable").await;
    let (rs_status, rs) = act(RUST, &rs_bot, "disable").await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go));
    assert_eq!(normalise(&go), normalise(&rs), "the disabled bots");
    assert!(rs.ends_with(b"\n"));

    let go_off = common::bot_and_user_rows(&go_bot).await.expect("rows");
    let rs_off = common::bot_and_user_rows(&rs_bot).await.expect("rows");
    for rows in [&go_off, &rs_off] {
        assert_ne!(rows["bot_delete_at"], 0, "the Bots row is soft-deleted");
        assert_ne!(
            rows["user_delete_at"], 0,
            "and so is the Users row — there is no `enabled` column"
        );
        assert_eq!(
            rows["user_delete_at"], rows["user_update_at"],
            "UpdateActive reads the clock once: DeleteAt *is* UpdateAt"
        );
        // A real clock reading, not a marker. `normalise` can only see that `delete_at` is
        // non-zero, so without this a port that wrote `1` — or the planted `CreateAt` — would
        // compare equal to Go's timestamp. The two stamps are separate `GetMillis()` calls
        // (`UpdateBotActive`'s, then `PreUpdate`'s), so they are close rather than equal.
        let deleted_at = rows["bot_delete_at"].as_i64().unwrap_or(0);
        let updated_at = rows["bot_update_at"].as_i64().unwrap_or(0);
        assert!(
            deleted_at > 1788600000000,
            "the Bots DeleteAt is a fresh timestamp, not a marker: {deleted_at}"
        );
        assert!(
            (deleted_at - updated_at).abs() < 5_000,
            "DeleteAt and UpdateAt are set microseconds apart: {deleted_at} vs {updated_at}"
        );
    }

    // --- disable again: the bot row must not move -------------------------------------------
    let (go_status, go) = act(GO, &go_bot, "disable").await;
    let (rs_status, rs) = act(RUST, &rs_bot, "disable").await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 200);
    assert_eq!(normalise(&go), normalise(&rs));

    let go_again = common::bot_and_user_rows(&go_bot).await.expect("rows");
    let rs_again = common::bot_and_user_rows(&rs_bot).await.expect("rows");
    for (before, after, server) in [(&go_off, &go_again, GO), (&rs_off, &rs_again, RUST)] {
        assert_eq!(
            before["bot_delete_at"], after["bot_delete_at"],
            "{server}: `changed` is false, so the Bots row is untouched"
        );
        assert_eq!(before["bot_update_at"], after["bot_update_at"], "{server}");
        assert!(
            after["user_update_at"].as_i64().unwrap_or(0)
                >= before["user_update_at"].as_i64().unwrap_or(0),
            "{server}: the Users row is written either way"
        );
    }

    // --- enable ------------------------------------------------------------------------------
    let (go_status, go) = act(GO, &go_bot, "enable").await;
    let (rs_status, rs) = act(RUST, &rs_bot, "enable").await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 200);
    assert_eq!(normalise(&go), normalise(&rs), "the re-enabled bots");
    let listed: serde_json::Value = serde_json::from_slice(&rs).expect("a bot");
    assert_eq!(listed["delete_at"], 0, "enable clears it outright");

    for bot in [&go_bot, &rs_bot] {
        let rows = common::bot_and_user_rows(bot).await.expect("rows");
        assert_eq!(rows["bot_delete_at"], 0);
        assert_eq!(rows["user_delete_at"], 0, "the Users row is restored too");
    }

    common::unplant_bot(&go_bot).await;
    common::unplant_bot(&rs_bot).await;
}

/// Assignment: the owner moves, `me` resolves to the session, an id matching nobody is accepted,
/// and a **bot** as the new owner is the one refusal.
#[tokio::test]
async fn assignment_moves_the_owner_and_refuses_only_a_bot() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();
    let team = create_team(&http, &admin, "bota").await;
    let user = create_plain_user(&http, &admin, &team, "bota").await;

    let Some((go_bot, rs_bot)) = two_bots("asgn", me).await else {
        return;
    };

    let assign = async |base: &str, bot: &str, to: &str| -> (u16, Vec<u8>) {
        send(
            &http,
            reqwest::Method::POST,
            base,
            &admin,
            &format!("/api/v4/bots/{bot}/assign/{to}"),
            None,
        )
        .await
    };

    // A plain user: a legitimate new owner.
    let (go_status, go) = assign(GO, &go_bot, &user.id).await;
    let (rs_status, rs) = assign(RUST, &rs_bot, &user.id).await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go));
    assert_eq!(normalise(&go), normalise(&rs));
    let owned: serde_json::Value = serde_json::from_slice(&rs).expect("a bot");
    assert_eq!(owned["owner_id"], user.id.as_str());

    // `me` is rewritten by `RequireUserId` before it is validated, so this hands the bot back to
    // the admin. Note the *bot* id has no such rule — `/bots/me/...` is a 400, below.
    let (go_status, go) = assign(GO, &go_bot, "me").await;
    let (rs_status, rs) = assign(RUST, &rs_bot, "me").await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 200);
    assert_eq!(normalise(&go), normalise(&rs));
    let owned: serde_json::Value = serde_json::from_slice(&rs).expect("a bot");
    assert_eq!(owned["owner_id"], me, "`me` is the session's own id");

    // **An id matching no user is accepted.** The only question asked of the new owner is whether
    // it is a bot, and `if user, err := GetUser(...); err == nil` answers no for a miss —
    // `Bots.OwnerId` legitimately holds plugin ids.
    const NOBODY: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let (go_status, go) = assign(GO, &go_bot, NOBODY).await;
    let (rs_status, rs) = assign(RUST, &rs_bot, NOBODY).await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 200, "a non-existent owner is not an error");
    assert_eq!(normalise(&go), normalise(&rs));
    let owned: serde_json::Value = serde_json::from_slice(&rs).expect("a bot");
    assert_eq!(owned["owner_id"], NOBODY);

    // A **bot** is refused. Go names `assign_bot` in the error — a permission nothing on this
    // route ever *checks*, since the gate above is `manage_bots`/`manage_others_bots` — but
    // `MakePermissionError`'s detail is not on the wire (`detailed_error` is empty on both
    // servers), so which permission it named is unobservable here. The status, the id and the
    // fact that the write did not happen are what a client can see, and all three are compared.
    let (go_status, go) = assign(GO, &go_bot, &rs_bot).await;
    let (rs_status, rs) = assign(RUST, &rs_bot, &go_bot).await;
    assert_eq!(rs_status, go_status);
    assert_eq!(go_status, 403, "a bot may not own a bot");
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, "assign to a bot");
    assert_eq!(parsed["id"], "api.context.permissions.app_error");

    // The refusal wrote nothing.
    assert_eq!(
        common::bot_and_user_rows(&rs_bot).await.expect("rows")["owner_id"],
        NOBODY
    );

    common::unplant_bot(&go_bot).await;
    common::unplant_bot(&rs_bot).await;
    common::delete_plain_user(&http, &admin, &user.id).await;
}

/// **The manage gate answers 404 or 403 depending on a permission the caller may not have heard
/// of**, and all four write routes share it.
///
/// A caller with no bot rights is told the bot does not exist — the same
/// `store.sql_bot.get.missing.app_error` a missing id gets, because a 403 would confirm the id is
/// a bot. A caller who may *read* others' bots but not manage them gets a real 403 naming
/// `manage_others_bots`. Neither branch is reachable with a stock role, so the role is planted.
#[tokio::test]
async fn the_manage_gate_hides_what_it_refuses_unless_the_caller_may_read() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _bots = common::BOT_FIXTURES.lock().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let me = common::logged_in_user_id();
    let team = create_team(&http, &admin, "botg").await;

    let Some(bot) = common::plant_bot("gate", me, 0).await else {
        return;
    };
    let before = common::bot_and_user_rows(&bot).await.expect("rows");

    // Every write route, so the cascade is asserted once per route rather than once.
    let routes: [(reqwest::Method, String, Option<&[u8]>); 4] = [
        (
            reqwest::Method::PUT,
            format!("/api/v4/bots/{bot}"),
            Some(br#"{"description":"stolen"}"#),
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/bots/{bot}/disable"),
            None,
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/bots/{bot}/enable"),
            None,
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/bots/{bot}/assign/{me}"),
            None,
        ),
    ];

    // --- no bot rights at all: the bot "does not exist" ---------------------------------------
    let plain = create_plain_user(&http, &admin, &team, "botg").await;
    for (method, path, body) in &routes {
        let ((go_status, go), (rs_status, rs)) =
            both(&http, method.clone(), &plain.token, path, *body).await;
        assert_eq!(rs_status, go_status, "{path}");
        assert_eq!(
            go_status, 404,
            "{path}: a plain caller is not told this is a bot"
        );
        let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        assert_eq!(
            parsed["id"], "store.sql_bot.get.missing.app_error",
            "{path}"
        );
    }

    // **The body is decoded before the permission is checked**, so a caller who is not even
    // allowed to know this is a bot gets a 400 for a malformed body rather than the 404. Go's
    // ordering, and the one place on this family where the hiding rule does not apply.
    let path = format!("/api/v4/bots/{bot}");
    let ((go_status, go), (rs_status, rs)) = both(
        &http,
        reqwest::Method::PUT,
        &plain.token,
        &path,
        Some(b"null"),
    )
    .await;
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(go_status, 400, "the body gate precedes the manage gate");
    let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(parsed["id"], "api.context.invalid_body_param.app_error");

    // --- `read_others_bots` without `manage_others_bots`: a real 403 --------------------------
    let Some(role) = common::plant_role("botgate", "read_others_bots").await else {
        common::unplant_bot(&bot).await;
        common::delete_plain_user(&http, &admin, &plain.id).await;
        return;
    };
    common::set_user_roles(&plain.id, &format!("system_user {role}")).await;
    let reader = common::login_plain_user(&http, "botg").await;

    for (method, path, body) in &routes {
        let ((go_status, go), (rs_status, rs)) =
            both(&http, method.clone(), &reader, path, *body).await;
        assert_eq!(rs_status, go_status, "{path}");
        assert_eq!(
            go_status,
            403,
            "{path}: a reader is refused, not hidden from: {}",
            String::from_utf8_lossy(&go)
        );
        let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        // Go names `manage_others_bots` here and we name it too, but `MakePermissionError`'s
        // detail does not reach the wire — so the observable difference between this refusal and
        // the 404 above is the **status**, which is exactly the leak the 404 exists to prevent
        // and exactly what is asserted.
        assert_eq!(parsed["id"], "api.context.permissions.app_error", "{path}");
        assert_eq!(parsed["detailed_error"], "", "{path}");
    }

    // Sixteen refusals, and not one write.
    assert_eq!(
        common::bot_and_user_rows(&bot).await.expect("rows"),
        before,
        "a refused write must leave both rows alone"
    );

    common::unplant_bot(&bot).await;
    common::delete_plain_user(&http, &admin, &plain.id).await;
}

/// `RequireBotUserId` and `RequireUserId` on the write routes, and the order between them.
#[tokio::test]
async fn the_write_routes_id_checks_agree() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    const GOOD: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

    let cases: [(reqwest::Method, String, &str, Option<&[u8]>); 5] = [
        (
            reqwest::Method::PUT,
            "/api/v4/bots/short".to_owned(),
            "bot_user_id",
            Some(br#"{"description":"x"}"#),
        ),
        (
            reqwest::Method::POST,
            "/api/v4/bots/short/disable".to_owned(),
            "bot_user_id",
            None,
        ),
        (
            reqwest::Method::POST,
            "/api/v4/bots/short/enable".to_owned(),
            "bot_user_id",
            None,
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/bots/{GOOD}/assign/short"),
            "user_id",
            None,
        ),
        // **Both ids malformed reports `user_id`**: `RequireUserId` runs first and
        // `RequireBotUserId` returns early once `c.Err` is set. Reversing the two is invisible
        // to every single-bad-id case above.
        (
            reqwest::Method::POST,
            "/api/v4/bots/short/assign/short".to_owned(),
            "user_id",
            None,
        ),
    ];

    for (method, path, name, body) in &cases {
        let ((go_status, go), (rs_status, rs)) =
            both(&http, method.clone(), &admin, path, *body).await;
        assert_eq!(rs_status, go_status, "{path}");
        assert_eq!(go_status, 400, "{path}");
        let parsed = common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");
        go_message_names(&go, name);
    }

    // Outside the mux charset, so gorilla never routed it and we forward: Go's own 404.
    let path = format!("/api/v4/bots/{GOOD}/assign/has.dot");
    let (go_status, go) = send(&http, reqwest::Method::POST, GO, &admin, &path, None).await;
    let response = http
        .post(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("reachable");
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let rs_status = response.status().as_u16();
    let rs = response.bytes().await.expect("reads").to_vec();
    assert_eq!(served_by.as_deref(), Some("go"), "{path} must be forwarded");
    assert_eq!(go_status, 404, "gorilla's NotFoundHandler");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
}
