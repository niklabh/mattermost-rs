//! Cross-server parity for `GET /api/v4/commands/{command_id}` and the `custom_only` half of
//! `GET /api/v4/commands`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity parity::commands
//! ```
//!
//! # `getCommand` answers 404 to everything, and that is what is under test
//!
//! Four conditions end in the same `SetCommandNotFoundError`: no such command, no `view_team`, no
//! `manage_own_slash_commands`, and neither the creator nor a holder of
//! `manage_others_slash_commands`. Go says why in a comment — a 403 would tell a caller that a
//! command id is real and which team it belongs to. So the tests below check that a caller who
//! *would* be refused gets a body indistinguishable from a miss, not merely that they are refused.
//!
//! # The other half of `listCommands` is forwarded
//!
//! Without `custom_only` the handler merges the **built-in** slash commands, which live in
//! `app/slashcommands/` with translated display names and are not ported. That branch is compared
//! here as a forwarded response, so the suite still fails if it ever stops being forwarded.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, assert_error_bodies_match_except_known_gaps, client,
    create_plain_user, create_team, fetch_both_raw, go_minted_token, logged_in_user_id,
    stack_enabled,
};

const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// Every test here plants into `Commands`, and `listCommands` lists a whole team.
static COMMANDS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A `Commands` row, written directly.
///
/// `POST /api/v4/commands` would work — `EnableCommands` is on — but it also mints a token and
/// sets `CreatorId` to the caller, and two of `getCommand`'s four refusals turn on the creator
/// being someone *else*. Planting is what makes that reachable.
async fn plant_command(
    tag: &str,
    team_id: &str,
    creator_id: &str,
    delete_at: i64,
) -> Option<String> {
    let pool = common::fixture_pool().await?;
    let id = format!("mmrscmd{tag:0>19}");
    sqlx::query(
        r#"
        INSERT INTO commands (id, token, createat, updateat, deleteat, creatorid, teamid,
                              "trigger", method, username, iconurl, autocomplete,
                              autocompletedesc, autocompletehint, displayname, description, url,
                              pluginid)
        VALUES ($1, $2, 1788600000000, 1788600000000, $3, $4, $5, $6, 'P', '', '', true,
                'planted by the parity suite', '[hint]', $6, '', 'https://example.invalid/hook', '')
        ON CONFLICT (id) DO UPDATE SET creatorid = EXCLUDED.creatorid,
                                       teamid = EXCLUDED.teamid,
                                       deleteat = EXCLUDED.deleteat
        "#,
    )
    .bind(&id)
    .bind(format!("mmrscmdtok{tag:0>16}"))
    .bind(delete_at)
    .bind(creator_id)
    .bind(team_id)
    .bind(format!("mmrs{tag}"))
    .execute(&pool)
    .await
    .expect("the command row is written");
    Some(id)
}

async fn unplant_commands() {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    sqlx::query("DELETE FROM commands WHERE id LIKE 'mmrscmd%'")
        .execute(&pool)
        .await
        .expect("the planted commands are removed");
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

/// The happy path, byte for byte — including the **token**, which `getCommand` does *not*
/// sanitise. That is worth pinning: the autocomplete listing calls `Sanitize()` and this route
/// does not, so the same row is two different documents depending on which route served it.
#[tokio::test]
async fn a_command_the_caller_created_matches_byte_for_byte() {
    if !stack_enabled() {
        return;
    }
    let _commands = COMMANDS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let team = create_team(&client, &token, "cmdown").await;

    let Some(planted) = plant_command("own", &team, logged_in_user_id(), 0).await else {
        return; // no DATABASE_URL
    };

    let path = format!("/api/v4/commands/{planted}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 200, "{path}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    assert!(rs.ends_with(b"\n"), "the encoder's newline");
    assert_eq!(
        served_by(&client, &token, &path).await.as_deref(),
        Some("rust")
    );

    let command: serde_json::Value = serde_json::from_slice(&rs).expect("json");
    assert_eq!(command["trigger"], "mmrsown");
    assert!(
        !command["token"].as_str().unwrap_or_default().is_empty(),
        "getCommand does not sanitise, so the token is on the wire: {command}"
    );
    assert_eq!(command["creator_id"], logged_in_user_id());
    // `autocomplete_data` and `autocomplete_icon_data` are `db:"-"` and absent from a stored row.
    assert!(command.get("autocomplete_data").is_none(), "{command}");

    unplant_commands().await;
}

/// **All four refusals are the same document.** A missing id, a command in a team the caller
/// cannot view, and a command the caller neither created nor may manage — compared against each
/// other, not just against a status code.
#[tokio::test]
async fn every_refusal_is_indistinguishable_from_a_miss() {
    if !stack_enabled() {
        return;
    }
    let _commands = COMMANDS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "cmdref").await;
    let user = create_plain_user(&client, &admin, &team, "cmdref").await;

    // A command in that team, created by the **admin**.
    let Some(theirs) = plant_command("ref", &team, logged_in_user_id(), 0).await else {
        return;
    };
    // A command in a team the plain user is not in at all.
    let other_team = create_team(&client, &admin, "cmdout").await;
    let Some(elsewhere) = plant_command("out", &other_team, logged_in_user_id(), 0).await else {
        return;
    };
    // A soft-deleted command, which the store's `DeleteAt = 0` predicate hides.
    let Some(deleted) = plant_command("del", &team, logged_in_user_id(), 1788600001000).await
    else {
        return;
    };

    let miss = format!("/api/v4/commands/{NOWHERE}");
    let ((miss_status, go_miss), (rs_status, rs_miss)) =
        fetch_both_raw(&client, &user.token, &miss).await;
    assert_eq!(miss_status, 404, "{miss}");
    assert_eq!(rs_status, miss_status);
    let miss_body = assert_error_bodies_match_except_known_gaps(&go_miss, &rs_miss, &miss);
    assert_eq!(miss_body["id"], "store.sql_command.save.get.app_error");

    for (path, why) in [
        (
            format!("/api/v4/commands/{theirs}"),
            "in a team the caller can view, but they may not manage slash commands there",
        ),
        (
            format!("/api/v4/commands/{elsewhere}"),
            "in a team the caller cannot view at all",
        ),
        (
            format!("/api/v4/commands/{deleted}"),
            "soft-deleted, so the store's predicate hides it",
        ),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &user.token, &path).await;
        assert_eq!(go_status, 404, "{path} ({why})");
        assert_eq!(rs_status, go_status, "{path}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(
            body["id"], miss_body["id"],
            "{path} ({why}): a refusal must be indistinguishable from a miss"
        );
        assert_eq!(body["status_code"], miss_body["status_code"], "{path}");
        assert_eq!(
            served_by(&client, &user.token, &path).await.as_deref(),
            Some("rust"),
            "{path}"
        );
    }

    // Even the **admin**, who can do everything else, is refused the soft-deleted one.
    let path = format!("/api/v4/commands/{deleted}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &admin, &path).await;
    assert_eq!(go_status, 404, "{path}");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go, &rs, &path);

    unplant_commands().await;
    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// **The creator filter, the soft-delete predicate and the team predicate**, in one fixture —
/// because each of them is invisible unless the *other* two rows are present to be excluded.
///
/// Four commands: two live in the team under different creators, one soft-deleted in the same
/// team, and one in a different team. An admin sees exactly the first two; a caller holding only
/// `manage_own_slash_commands` sees exactly their own.
#[tokio::test]
async fn custom_only_narrows_to_the_creator_unless_the_caller_manages_others() {
    if !stack_enabled() {
        return;
    }
    let _commands = COMMANDS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "cmdlist").await;
    let elsewhere = create_team(&client, &admin, "cmdlist2").await;

    let Some(author_role) = common::plant_role("cmdauthor", "manage_own_slash_commands").await
    else {
        return; // no DATABASE_URL
    };
    let user = create_plain_user(&client, &admin, &team, "cmdlist").await;
    common::set_user_roles(&user.id, &format!("system_user {author_role}")).await;
    let author = common::login_plain_user(&client, "cmdlist").await;

    let Some(theirs) = plant_command("lista", &team, logged_in_user_id(), 0).await else {
        return;
    };
    let Some(mine) = plant_command("listb", &team, &user.id, 0).await else {
        return;
    };
    // Excluded by `DeleteAt = 0`…
    let Some(_deleted) = plant_command("listc", &team, &user.id, 1788600001000).await else {
        return;
    };
    // …and by `TeamId = $1`. Both belong to the caller, so only the predicate can exclude them.
    let Some(_other_team) = plant_command("listd", &elsewhere, &user.id, 0).await else {
        return;
    };

    let ids = |body: &[u8]| -> Vec<String> {
        let parsed: serde_json::Value = serde_json::from_slice(body).expect("an array");
        let mut names: Vec<String> = parsed
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(|c| c["id"].as_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    };

    // The admin holds `manage_others_slash_commands`, so the filter is cleared: both live
    // commands in this team, and neither the deleted one nor the other team's.
    let path = format!("/api/v4/commands?team_id={team}&custom_only=true");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &admin, &path).await;
    assert_eq!(go_status, 200, "{path}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    let mut both = vec![theirs.clone(), mine.clone()];
    both.sort();
    assert_eq!(
        ids(&rs),
        both,
        "an admin sees this team's live commands and nothing else"
    );
    assert!(rs.ends_with(b"\n"), "the encoder's newline");
    assert_eq!(
        served_by(&client, &admin, &path).await.as_deref(),
        Some("rust")
    );

    // **`manage_own` without `manage_others`**: the filter is the caller's own id, so one command.
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &author, &path).await;
    assert_eq!(go_status, 200, "{path}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{path}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    assert_eq!(
        ids(&rs),
        vec![mine.clone()],
        "the filter is the caller's id, so the admin's command is not listed"
    );

    // A caller with neither permission is refused outright — the gate the filter sits behind.
    let plain = create_plain_user(&client, &admin, &team, "cmdplain").await;
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &plain.token, &path).await;
    assert_eq!(go_status, 403, "{path}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status);
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(body["id"], "api.context.permissions.app_error");

    unplant_commands().await;
    common::delete_plain_user(&client, &admin, &user.id).await;
    common::delete_plain_user(&client, &admin, &plain.id).await;
}

/// **Each of `getCommand`'s three permission gates, refused on its own.**
///
/// A caller who fails all three proves only that *some* gate refuses — and four mutations that
/// removed one gate each survived the first run of this suite for exactly that reason. These three
/// callers each fail exactly one gate and pass the others, so removing that gate turns the 404
/// into a 200.
#[tokio::test]
async fn each_gate_refuses_on_its_own() {
    if !stack_enabled() {
        return;
    }
    let _commands = COMMANDS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "cmdgates").await;
    let elsewhere = create_team(&client, &admin, "cmdgates2").await;

    let Some(manager_role) = common::plant_role(
        "cmdmanager",
        "manage_own_slash_commands manage_others_slash_commands",
    )
    .await
    else {
        return;
    };
    let Some(others_role) = common::plant_role("cmdothers", "manage_others_slash_commands").await
    else {
        return;
    };
    let Some(author_role2) = common::plant_role("cmdauthor2", "manage_own_slash_commands").await
    else {
        return;
    };

    // 1. Fails **only `view_team`**: holds both command permissions, but is a member of a
    //    different team — and `view_team` comes from the team membership, not from `system_user`.
    let outsider = create_plain_user(&client, &admin, &elsewhere, "cmdout1").await;
    common::set_user_roles(&outsider.id, &format!("system_user {manager_role}")).await;
    let outsider_token = common::login_plain_user(&client, "cmdout1").await;

    // 2. Fails **only `manage_own_slash_commands`**: in the team, and holds `manage_others`.
    let no_own = create_plain_user(&client, &admin, &team, "cmdnoown").await;
    common::set_user_roles(&no_own.id, &format!("system_user {others_role}")).await;
    let no_own_token = common::login_plain_user(&client, "cmdnoown").await;

    // 3. Fails **only the creator check**: in the team with `manage_own` and not `manage_others`,
    //    asking about a command someone else created.
    let author = create_plain_user(&client, &admin, &team, "cmdauth").await;
    common::set_user_roles(&author.id, &format!("system_user {author_role2}")).await;
    let author_token = common::login_plain_user(&client, "cmdauth").await;

    let Some(admins) = plant_command("gate", &team, logged_in_user_id(), 0).await else {
        return;
    };
    let Some(authors) = plant_command("gateb", &team, &author.id, 0).await else {
        return;
    };

    let path = format!("/api/v4/commands/{admins}");
    for (label, token) in [
        ("no view_team", &outsider_token),
        ("no manage_own_slash_commands", &no_own_token),
        ("not the creator", &author_token),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, token, &path).await;
        assert_eq!(go_status, 404, "{label}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status, "{label}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(
            body["id"], "store.sql_command.save.get.app_error",
            "{label}"
        );
        assert_eq!(
            served_by(&client, token, &path).await.as_deref(),
            Some("rust"),
            "{label}"
        );
    }

    // **The positive control.** The same author, on the command they *did* create, is admitted —
    // so the 404 above is the creator check and not a blanket refusal, and inverting that check
    // would break this half instead.
    let own = format!("/api/v4/commands/{authors}");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &author_token, &own).await;
    assert_eq!(go_status, 200, "{own}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status, "{own}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{own}"
    );

    // And a caller holding `manage_others` *in the team* reads the admin's command — the third
    // gate's grant, which is what makes "not the creator" above about the creator.
    let manager = create_plain_user(&client, &admin, &team, "cmdmgr").await;
    common::set_user_roles(&manager.id, &format!("system_user {manager_role}")).await;
    let manager_token = common::login_plain_user(&client, "cmdmgr").await;
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &manager_token, &path).await;
    assert_eq!(go_status, 200, "{path}: {}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );

    unplant_commands().await;
    for id in [&outsider.id, &no_own.id, &author.id, &manager.id] {
        common::delete_plain_user(&client, &admin, id).await;
    }
}

/// The two gates before the branch, and the parameter error that is spelled `body` for a query
/// parameter.
#[tokio::test]
async fn the_team_id_and_view_team_gates_agree() {
    if !stack_enabled() {
        return;
    }
    let _commands = COMMANDS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "cmdgate").await;
    let user = create_plain_user(&client, &admin, &team, "cmdgate").await;

    // No `team_id` at all — the **body**-param error, before any permission question.
    for path in ["/api/v4/commands", "/api/v4/commands?custom_only=true"] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &admin, path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        assert_eq!(
            body["id"], "api.context.invalid_body_param.app_error",
            "{path}"
        );
        assert_eq!(
            served_by(&client, &admin, path).await.as_deref(),
            Some("rust"),
            "{path}"
        );
    }

    // A team the caller is not in: `view_team` refuses, before the branch — so both spellings of
    // the query give the same 403 and neither reaches the built-in registry.
    let other_team = create_team(&client, &admin, "cmdgate2").await;
    for query in ["", "&custom_only=true"] {
        let path = format!("/api/v4/commands?team_id={other_team}{query}");
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &user.token, &path).await;
        assert_eq!(go_status, 403, "{path}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, go_status, "{path}");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(body["id"], "api.context.permissions.app_error", "{path}");
        assert_eq!(
            served_by(&client, &user.token, &path).await.as_deref(),
            Some("rust"),
            "{path}: the gate is ours even on the forwarded branch"
        );
    }

    // `RequireCommandId`'s 400 — the **url**-param error this time.
    let path = "/api/v4/commands/short";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &admin, path).await;
    assert_eq!(go_status, 400, "{path}");
    assert_eq!(rs_status, go_status);
    let body = assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");

    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// **The built-in branch is forwarded**, and this is what fails if it ever stops being.
#[tokio::test]
async fn the_built_in_command_branch_reaches_go() {
    if !stack_enabled() {
        return;
    }
    let _commands = COMMANDS.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let team = create_team(&client, &token, "cmdfwd").await;

    let path = format!("/api/v4/commands?team_id={team}");
    let response = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("we answer");
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("reads").to_vec();

    assert_eq!(
        served.as_deref(),
        Some("go"),
        "{path}: the built-in slash commands are not ported"
    );
    assert_eq!(status, 200);

    let theirs = client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .bytes()
        .await
        .expect("reads")
        .to_vec();

    // **Compared as a set, and that is not a concession — it is what Go answers.**
    // `ListAllCommandsByUser` ranges over `commandProviders`, a Go **map**, so the order of the
    // built-ins is randomised per call: two consecutive reads of the *same* Go server disagree
    // byte for byte. Measured — the first version of this assertion compared the forwarded body
    // against a second Go read and failed on the order alone. What a forward guarantees is that
    // the bytes came from Go, not that Go is deterministic.
    let triggers = |raw: &[u8]| -> Vec<String> {
        let parsed: serde_json::Value = serde_json::from_slice(raw).expect("an array");
        let mut names: Vec<String> = parsed
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(|c| c["trigger"].as_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    };
    assert_eq!(
        triggers(&theirs),
        triggers(&body),
        "a forwarded body is Go's own, up to the order Go itself does not fix"
    );

    // And it really does carry built-ins, which is why it cannot be served from the database.
    let listed: serde_json::Value = serde_json::from_slice(&body).expect("an array");
    assert!(
        listed
            .as_array()
            .expect("an array")
            .iter()
            .any(|c| c["id"].as_str() == Some("")),
        "a built-in command has no stored id: {listed}"
    );
}
