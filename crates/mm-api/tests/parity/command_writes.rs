//! Cross-server parity for the five slash-command writes: `createCommand`, `updateCommand`,
//! `deleteCommand`, `moveCommand` and `regenCommandToken`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity parity::command_writes
//! ```
//!
//! # The built-in trigger list is checked against the running Go server, not read off
//!
//! `validateCommandTriggerUniqueness` compares a proposed trigger against `commandProviders`, a
//! registry of ~35 built-in providers that this port replaces with a list of 33 strings. A list is
//! a transcription, and a transcription is exactly the kind of thing that is quietly wrong — a
//! dropped entry lets a client register `/help` and shadow a built-in. So
//! [`the_built_in_triggers_are_reserved_by_both_servers`] posts **every** entry to both servers
//! and demands the same refusal from each, and posts the two triggers whose providers return
//! `nil` on a stock server and demands the same acceptance. That is the registry itself as the
//! oracle, which is the only honest one available without linking Go.
//!
//! # Each server writes its own rows
//!
//! One database, two servers: a command created through Go is visible to us and would make our
//! own create a duplicate trigger. Every test that writes therefore gives each server its own
//! team, and reads back through the server that wrote ([D-190]).

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, assert_error_bodies_match_except_known_gaps, client,
    create_plain_user, create_team, go_minted_token, logged_in_user_id, stack_enabled,
};

/// Serialises this module against itself. The writes are heavy and every one of them lists or
/// scans a team's commands.
static COMMAND_WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The 33 triggers `App::built_in_command_triggers` reserves unconditionally, transcribed here
/// **independently** of the port's own constant — a shared constant would let one typo satisfy
/// both sides.
const BUILT_IN: &[&str] = &[
    "away",
    "code",
    "collapse",
    "dnd",
    "echo",
    "expand",
    "groupmsg",
    "header",
    "help",
    "invite",
    "invite_people",
    "join",
    "kick",
    "leave",
    "logout",
    "marketplace",
    "me",
    "mobile-logs",
    "msg",
    "mute",
    "offline",
    "online",
    "open",
    "purpose",
    "remove",
    "rename",
    "search",
    "secure-connection",
    "settings",
    "share-channel",
    "shortcuts",
    "shrug",
    "status",
];

/// The two whose providers return `nil` on a stock server — `/test` needs
/// `ServiceSettings.EnableTesting` and `/exportlink` needs a feature flag plus a dedicated export
/// store. Neither holds here, so both must be **accepted** as custom triggers by both servers.
const FREE_ON_A_STOCK_SERVER: &[&str] = &["test", "exportlink"];

async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    base: &str,
    token: &str,
    path: &str,
    body: Option<&serde_json::Value>,
) -> (u16, String) {
    let mut request = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

async fn post(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, String) {
    send(http, reqwest::Method::POST, base, token, path, Some(body)).await
}

async fn put(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, String) {
    send(http, reqwest::Method::PUT, base, token, path, Some(body)).await
}

async fn put_empty(http: &reqwest::Client, base: &str, token: &str, path: &str) -> (u16, String) {
    send(http, reqwest::Method::PUT, base, token, path, None).await
}

async fn delete(http: &reqwest::Client, base: &str, token: &str, path: &str) -> (u16, String) {
    send(http, reqwest::Method::DELETE, base, token, path, None).await
}

async fn get(http: &reqwest::Client, base: &str, token: &str, path: &str) -> (u16, String) {
    send(http, reqwest::Method::GET, base, token, path, None).await
}

/// A minimal valid create body. `url` carries an `&` deliberately: `encoding/json` escapes it to
/// `&` and `serde_json` does not, so a port that marshals with serde rather than Go's
/// escaping differs from Go on this field and on no other.
fn create_body(team: &str, trigger: &str) -> serde_json::Value {
    serde_json::json!({
        "team_id": team,
        "trigger": trigger,
        "method": "P",
        "url": "https://example.invalid/hook?a=1&b=2",
        "display_name": "mmrs parity <command>",
        "description": "planted by the parity suite",
        "auto_complete": true,
        "auto_complete_desc": "a description",
        "auto_complete_hint": "[hint]",
    })
}

/// Everything a command's id, token and timestamps make per-server, reduced to "present" or
/// "non-zero" so two independently created commands can be compared field by field.
fn normalise(command: &serde_json::Value) -> serde_json::Value {
    let mut out = command.clone();
    let Some(object) = out.as_object_mut() else {
        return out;
    };
    for key in ["id", "token", "team_id"] {
        if let Some(value) = object.get(key) {
            let present = value.as_str().is_some_and(|s| s.len() == 26);
            object.insert(key.to_owned(), serde_json::json!(present));
        }
    }
    for key in ["create_at", "update_at"] {
        if let Some(value) = object.get(key) {
            object.insert(key.to_owned(), serde_json::json!(value.as_i64() > Some(0)));
        }
    }
    out
}

/// Plant a `Commands` row with an arbitrary creator — the one thing `POST /commands` cannot do
/// without `manage_others_slash_commands` *and* the creator being a real user with access.
async fn plant_command(tag: &str, team_id: &str, creator_id: &str) -> Option<String> {
    let pool = common::fixture_pool().await?;
    let id = format!("mmrscw{tag:0>20}");
    sqlx::query(
        r#"
        INSERT INTO commands (id, token, createat, updateat, deleteat, creatorid, teamid,
                              "trigger", method, username, iconurl, autocomplete,
                              autocompletedesc, autocompletehint, displayname, description, url,
                              pluginid)
        VALUES ($1, $2, 1788600000000, 1788600000000, 0, $3, $4, $5, 'P', '', '', true,
                'planted by the parity suite', '[hint]', $5, '',
                'https://example.invalid/hook', '')
        ON CONFLICT (id) DO UPDATE SET creatorid = EXCLUDED.creatorid,
                                       teamid = EXCLUDED.teamid,
                                       deleteat = 0
        "#,
    )
    .bind(&id)
    .bind(format!("mmrscwtok{tag:0>17}"))
    .bind(creator_id)
    .bind(team_id)
    .bind(format!("mmrs{tag}"))
    .execute(&pool)
    .await
    .expect("the command row is written");
    Some(id)
}

/// The id of the live command a team holds under `trigger`, if any — the only way to name a
/// command created through the *other* server without parsing its response again.
async fn occupied(team: &str, trigger: &str) -> Option<String> {
    let pool = common::fixture_pool().await?;
    let row: Option<(String,)> = sqlx::query_as(
        r#"SELECT id FROM commands WHERE teamid = $1 AND "trigger" = $2 AND deleteat = 0"#,
    )
    .bind(team)
    .bind(trigger)
    .fetch_optional(&pool)
    .await
    .expect("the lookup runs");
    row.map(|row| row.0)
}

/// Remove everything this module wrote — the planted rows *and* the ones the servers created, by
/// team, since those carry server-minted ids.
async fn sweep(teams: &[&str]) {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    sqlx::query("DELETE FROM commands WHERE id LIKE 'mmrscw%' OR teamid = ANY($1)")
        .bind(teams)
        .execute(&pool)
        .await
        .expect("the commands are removed");
}

fn parse(raw: &str) -> serde_json::Value {
    serde_json::from_str(raw).unwrap_or_else(|e| panic!("not JSON: {raw:?} ({e})"))
}

/// **The happy path on both servers, compared field by field**, plus the read-back that proves
/// the row and not just the response.
///
/// The `url` and `display_name` both contain characters `encoding/json` escapes, which is what
/// makes this a byte comparison worth making rather than a shape check.
#[tokio::test]
async fn a_created_command_matches_go_field_for_field() {
    if !stack_enabled() {
        return;
    }
    let _writes = COMMAND_WRITES.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let go_team = create_team(&http, &token, "cwcreg").await;
    let rust_team = create_team(&http, &token, "cwcrer").await;

    let (go_status, go_raw) = post(
        &http,
        GO,
        &token,
        "/api/v4/commands",
        &create_body(&go_team, "MmrsCreate"),
    )
    .await;
    let (rs_status, rs_raw) = post(
        &http,
        RUST,
        &token,
        "/api/v4/commands",
        &create_body(&rust_team, "MmrsCreate"),
    )
    .await;

    assert_eq!(go_status, 201, "Go answers Created: {go_raw}");
    assert_eq!(rs_status, go_status, "the create status differs: {rs_raw}");
    assert!(go_raw.ends_with('\n'), "Go's encoder framing: {go_raw:?}");
    assert!(rs_raw.ends_with('\n'), "ours must too: {rs_raw:?}");

    // **`encoding/json` escapes `&`, `<` and `>` to `\u0026`, `\u003c`, `\u003e`.** Asserted on
    // the raw text, because `serde_json::Value` equality unescapes and would hide exactly this —
    // and `serde_json::to_string` emits the characters literally, so a port that marshalled with
    // serde passes every structural assertion in this test and fails these three.
    for escape in [r"\u0026", r"\u003c", r"\u003e"] {
        assert!(
            go_raw.contains(escape),
            "the fixture must reach the escaping branch for {escape}: {go_raw}"
        );
        assert!(
            rs_raw.contains(escape),
            "we must escape {escape} the way Go does:\n go: {go_raw}\nrust: {rs_raw}"
        );
    }
    assert!(
        !rs_raw.contains('&') && !rs_raw.contains('<'),
        "and emit no unescaped ones: {rs_raw}"
    );

    let go_command = parse(&go_raw);
    let rs_command = parse(&rs_raw);
    assert_eq!(
        normalise(&go_command),
        normalise(&rs_command),
        "the created command differs:\n go: {go_command}\nrust: {rs_command}"
    );

    // **The trigger is lower-cased before it is stored**, so the answer is not the request.
    assert_eq!(rs_command["trigger"], "mmrscreate");
    assert_eq!(go_command["trigger"], rs_command["trigger"]);
    assert_eq!(rs_command["creator_id"], logged_in_user_id());
    assert_eq!(
        rs_command["create_at"], rs_command["update_at"],
        "PreSave sets both from one GetMillis"
    );
    assert_eq!(rs_command["delete_at"], 0);
    // `db:"-"` on both, and `omitempty`, so neither key is on the wire.
    assert!(rs_command.get("autocomplete_data").is_none());
    assert!(rs_command.get("autocomplete_icon_data").is_none());
    assert_eq!(
        go_command.as_object().map(serde_json::Map::len),
        rs_command.as_object().map(serde_json::Map::len)
    );

    // Read back through the server that wrote, which is what proves the `INSERT`'s column order
    // rather than the struct the handler marshalled.
    let id = rs_command["id"].as_str().expect("an id");
    let (status, raw) = get(&http, RUST, &token, &format!("/api/v4/commands/{id}")).await;
    assert_eq!(status, 200, "our command reads back: {raw}");
    let stored = parse(&raw);
    assert_eq!(stored["trigger"], "mmrscreate");
    assert_eq!(stored["url"], "https://example.invalid/hook?a=1&b=2");
    assert_eq!(stored["display_name"], "mmrs parity <command>");
    assert_eq!(stored["auto_complete_hint"], "[hint]");
    assert_eq!(stored["token"], rs_command["token"]);

    sweep(&[&go_team, &rust_team]).await;
}

/// **The built-in registry, as an oracle.** Every reserved trigger refused by both servers with
/// the same body; every trigger whose provider is switched off accepted by both.
///
/// A missing entry in the port's list shows up here as a 201 where Go gives a 400 — the failure a
/// unit test over a transcribed constant cannot produce.
#[tokio::test]
async fn the_built_in_triggers_are_reserved_by_both_servers() {
    if !stack_enabled() {
        return;
    }
    let _writes = COMMAND_WRITES.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let go_team = create_team(&http, &token, "cwbig").await;
    let rust_team = create_team(&http, &token, "cwbir").await;

    for trigger in BUILT_IN {
        let (go_status, go_raw) = post(
            &http,
            GO,
            &token,
            "/api/v4/commands",
            &create_body(&go_team, trigger),
        )
        .await;
        let (rs_status, rs_raw) = post(
            &http,
            RUST,
            &token,
            "/api/v4/commands",
            &create_body(&rust_team, trigger),
        )
        .await;

        assert_eq!(
            go_status, 400,
            "/{trigger} must be reserved by Go: {go_raw}"
        );
        assert_eq!(
            rs_status, go_status,
            "/{trigger}: Go says {go_status}, we say {rs_status}: {rs_raw}"
        );
        let body = assert_error_bodies_match_except_known_gaps(
            go_raw.as_bytes(),
            rs_raw.as_bytes(),
            trigger,
        );
        assert_eq!(body["id"], "api.command.duplicate_trigger.app_error");
    }

    // **The casing is folded before the comparison**, on both sides.
    for trigger in ["HELP", "Away", "sTaTuS"] {
        let (go_status, go_raw) = post(
            &http,
            GO,
            &token,
            "/api/v4/commands",
            &create_body(&go_team, trigger),
        )
        .await;
        let (rs_status, rs_raw) = post(
            &http,
            RUST,
            &token,
            "/api/v4/commands",
            &create_body(&rust_team, trigger),
        )
        .await;
        assert_eq!(go_status, 400, "/{trigger}: {go_raw}");
        assert_eq!(rs_status, go_status, "/{trigger}: {rs_raw}");
    }

    // And the two that are *not* reserved on a stock server, which is the other half of the
    // oracle: a port that reserved all 35 would pass every assertion above and fail these.
    for trigger in FREE_ON_A_STOCK_SERVER {
        let (go_status, go_raw) = post(
            &http,
            GO,
            &token,
            "/api/v4/commands",
            &create_body(&go_team, trigger),
        )
        .await;
        let (rs_status, rs_raw) = post(
            &http,
            RUST,
            &token,
            "/api/v4/commands",
            &create_body(&rust_team, trigger),
        )
        .await;
        assert_eq!(
            go_status, 201,
            "/{trigger}'s provider returns nil on a stock server, so Go accepts it: {go_raw}"
        );
        assert_eq!(rs_status, go_status, "/{trigger}: {rs_raw}");
        assert_eq!(
            normalise(&parse(&go_raw)),
            normalise(&parse(&rs_raw)),
            "/{trigger}"
        );
    }

    // A custom trigger collides with itself on the second create, and gives the **same** error as
    // a built-in collision — including across a case change, since the stored one was folded.
    let (rs_status, rs_raw) = post(
        &http,
        RUST,
        &token,
        "/api/v4/commands",
        &create_body(&rust_team, "MmrsDup"),
    )
    .await;
    assert_eq!(rs_status, 201, "{rs_raw}");
    for second in ["mmrsdup", "MMRSDUP", "MmrsDup"] {
        let (rs_status, rs_raw) = post(
            &http,
            RUST,
            &token,
            "/api/v4/commands",
            &create_body(&rust_team, second),
        )
        .await;
        assert_eq!(rs_status, 400, "/{second} is a duplicate: {rs_raw}");
        assert_eq!(
            parse(&rs_raw)["id"],
            "api.command.duplicate_trigger.app_error"
        );
    }
    let (go_status, go_raw) = post(
        &http,
        GO,
        &token,
        "/api/v4/commands",
        &create_body(&go_team, "MmrsDup"),
    )
    .await;
    assert_eq!(go_status, 201, "{go_raw}");
    let (go_status, go_raw) = post(
        &http,
        GO,
        &token,
        "/api/v4/commands",
        &create_body(&go_team, "mmrsdup"),
    )
    .await;
    assert_eq!(go_status, 400, "Go folds the case too: {go_raw}");

    sweep(&[&go_team, &rust_team]).await;
}

/// `createCommand`'s three refusals that are not about a trigger.
#[tokio::test]
async fn the_create_bodies_that_are_refused() {
    if !stack_enabled() {
        return;
    }
    let _writes = COMMAND_WRITES.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let go_team = create_team(&http, &admin, "cwbadg").await;
    let rust_team = create_team(&http, &admin, "cwbadr").await;
    let user = create_plain_user(&http, &admin, &rust_team, "cwbad").await;

    let pairs: Vec<(&str, serde_json::Value, serde_json::Value)> = vec![
        // **An id in the body is a 500**, not the 400 its shape suggests: the store's
        // `ErrInvalidInput` is not an `*model.AppError`, so `errors.As` misses it.
        (
            "an id in the body",
            {
                let mut b = create_body(&go_team, "mmrsid");
                b["id"] = serde_json::json!("kh9x6ffcbir9uy8ta8m1yprkga");
                b
            },
            {
                let mut b = create_body(&rust_team, "mmrsid");
                b["id"] = serde_json::json!("kh9x6ffcbir9uy8ta8m1yprkga");
                b
            },
        ),
        // `IsValid`'s url branch, which runs inside the store after `PreSave`.
        (
            "no url",
            {
                let mut b = create_body(&go_team, "mmrsnourl");
                b["url"] = serde_json::json!("");
                b
            },
            {
                let mut b = create_body(&rust_team, "mmrsnourl");
                b["url"] = serde_json::json!("");
                b
            },
        ),
        // A `plugin_id` plus the `creator_id` the handler always stamps — the both-set branch.
        (
            "a plugin_id",
            {
                let mut b = create_body(&go_team, "mmrsplug");
                b["plugin_id"] = serde_json::json!("com.mattermost.mmrs");
                b
            },
            {
                let mut b = create_body(&rust_team, "mmrsplug");
                b["plugin_id"] = serde_json::json!("com.mattermost.mmrs");
                b
            },
        ),
        // A method that is neither `G` nor `P`.
        (
            "an unknown method",
            {
                let mut b = create_body(&go_team, "mmrsmeth");
                b["method"] = serde_json::json!("X");
                b
            },
            {
                let mut b = create_body(&rust_team, "mmrsmeth");
                b["method"] = serde_json::json!("X");
                b
            },
        ),
    ];

    for (label, go_body, rust_body) in pairs {
        let (go_status, go_raw) = post(&http, GO, &admin, "/api/v4/commands", &go_body).await;
        let (rs_status, rs_raw) = post(&http, RUST, &admin, "/api/v4/commands", &rust_body).await;
        assert_eq!(
            rs_status, go_status,
            "{label}: Go {go_status}, us {rs_status}\n go: {go_raw}\nrust: {rs_raw}"
        );
        let body = assert_error_bodies_match_except_known_gaps(
            go_raw.as_bytes(),
            rs_raw.as_bytes(),
            label,
        );
        assert_ne!(body["id"], serde_json::Value::Null, "{label}");
    }

    // A body that is not JSON at all — `SetInvalidParamWithErr("command")`.
    for base in [GO, RUST] {
        let response = http
            .post(format!("{base}/api/v4/commands"))
            .header("Authorization", format!("Bearer {admin}"))
            .header("Content-Type", "application/json")
            .body("not json")
            .send()
            .await
            .expect("a response");
        assert_eq!(response.status().as_u16(), 400, "{base}");
        let raw = response.text().await.expect("a body");
        assert_eq!(
            parse(&raw)["id"],
            "api.context.invalid_body_param.app_error",
            "{base}: {raw}"
        );
    }

    // No `manage_own_slash_commands` anywhere: `system_user` grants it in no team, so a plain
    // member of the team is refused at the first gate with a plain 403.
    let (go_status, go_raw) = post(
        &http,
        GO,
        &user.token,
        "/api/v4/commands",
        &create_body(&rust_team, "mmrsnoperm"),
    )
    .await;
    let (rs_status, rs_raw) = post(
        &http,
        RUST,
        &user.token,
        "/api/v4/commands",
        &create_body(&rust_team, "mmrsnoperm"),
    )
    .await;
    assert_eq!(go_status, 403, "{go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    let body = assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rs_raw.as_bytes(),
        "no perm",
    );
    assert_eq!(body["id"], "api.context.permissions.app_error");

    // **An empty `team_id` is a 403, not a validation error** — `SessionHasPermissionToTeam`
    // returns false for `""` *before* consulting the admin's system roles, so even an admin
    // cannot reach `model.command.is_valid.team_id.app_error` through this route.
    let mut no_team = create_body(&rust_team, "mmrsnoteam");
    no_team["team_id"] = serde_json::json!("");
    let (go_status, go_raw) = post(&http, GO, &admin, "/api/v4/commands", &no_team).await;
    let (rs_status, rs_raw) = post(&http, RUST, &admin, "/api/v4/commands", &no_team).await;
    assert_eq!(go_status, 403, "an admin, and still a 403: {go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    assert_error_bodies_match_except_known_gaps(go_raw.as_bytes(), rs_raw.as_bytes(), "no team_id");

    sweep(&[&go_team, &rust_team]).await;
    common::delete_plain_user(&http, &admin, &user.id).await;
}

/// `updateCommand`: the fields the body cannot move, and the two 400s before the ladder.
#[tokio::test]
async fn an_update_keeps_the_old_id_token_creator_and_create_at() {
    if !stack_enabled() {
        return;
    }
    let _writes = COMMAND_WRITES.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let go_team = create_team(&http, &token, "cwupdg").await;
    let rust_team = create_team(&http, &token, "cwupdr").await;
    let other_team = create_team(&http, &token, "cwupdo").await;

    let mut created = Vec::new();
    for (base, team) in [(GO, &go_team), (RUST, &rust_team)] {
        let (status, raw) = post(
            &http,
            base,
            &token,
            "/api/v4/commands",
            &create_body(team, "mmrsupd"),
        )
        .await;
        assert_eq!(status, 201, "{base}: {raw}");
        created.push(parse(&raw));
    }
    let (go_command, rs_command) = (&created[0], &created[1]);

    // Everything a client could try to move, moved.
    let update = |original: &serde_json::Value, team: &str| {
        serde_json::json!({
            "id": original["id"],
            "team_id": team,
            "trigger": "MmrsUpdated",
            "method": "G",
            "url": "https://example.invalid/other?x=1&y=2",
            "display_name": "renamed",
            "description": "edited",
            "auto_complete": false,
            "auto_complete_desc": "edited desc",
            "auto_complete_hint": "[edited]",
            "username": "someone",
            "icon_url": "https://example.invalid/icon.png",
            // None of these four may survive.
            "token": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
            "creator_id": "bbbbbbbbbbbbbbbbbbbbbbbbbb",
            "create_at": 1,
            "delete_at": 99,
        })
    };

    let go_path = format!(
        "/api/v4/commands/{}",
        go_command["id"].as_str().unwrap_or("")
    );
    let rs_path = format!(
        "/api/v4/commands/{}",
        rs_command["id"].as_str().unwrap_or("")
    );
    let (go_status, go_raw) = put(&http, GO, &token, &go_path, &update(go_command, &go_team)).await;
    let (rs_status, rs_raw) = put(
        &http,
        RUST,
        &token,
        &rs_path,
        &update(rs_command, &rust_team),
    )
    .await;

    assert_eq!(
        go_status, 200,
        "a PUT that answers OK, not Created: {go_raw}"
    );
    assert_eq!(rs_status, go_status, "{rs_raw}");
    assert!(go_raw.ends_with('\n') && rs_raw.ends_with('\n'));

    let go_updated = parse(&go_raw);
    let rs_updated = parse(&rs_raw);
    assert_eq!(
        normalise(&go_updated),
        normalise(&rs_updated),
        "the updated command differs:\n go: {go_updated}\nrust: {rs_updated}"
    );

    for (updated, original) in [(&go_updated, go_command), (&rs_updated, rs_command)] {
        assert_eq!(updated["id"], original["id"]);
        assert_eq!(updated["token"], original["token"], "the token is copied");
        assert_eq!(
            updated["creator_id"], original["creator_id"],
            "the creator is copied"
        );
        assert_eq!(
            updated["create_at"], original["create_at"],
            "create_at is copied"
        );
        assert_eq!(updated["delete_at"], 0, "delete_at is copied, not taken");
        assert_eq!(updated["trigger"], "mmrsupdated", "and lower-cased");
        assert_eq!(updated["method"], "G");
        assert_eq!(updated["display_name"], "renamed");
        assert!(
            updated["update_at"].as_i64() > original["update_at"].as_i64(),
            "update_at moves: {updated}"
        );
    }

    // The row, not the answer — the response is marshalled from the struct the app assembled and
    // would be identical whatever the `UPDATE` wrote.
    let (status, raw) = get(&http, RUST, &token, &rs_path).await;
    assert_eq!(status, 200, "{raw}");
    let stored = parse(&raw);
    assert_eq!(stored["trigger"], "mmrsupdated");
    assert_eq!(stored["description"], "edited");
    assert_eq!(stored["display_name"], "renamed");
    assert_eq!(stored["auto_complete_desc"], "edited desc");
    assert_eq!(stored["username"], "someone");
    assert_eq!(stored["url"], "https://example.invalid/other?x=1&y=2");
    assert_eq!(stored["auto_complete"], false);
    assert_eq!(stored["update_at"], rs_updated["update_at"]);

    // **Re-saving a command under its own trigger is allowed**, and that is the whole job of
    // `excludeCommandID`: without it the command collides with itself and every update is a
    // `duplicate_trigger` 400.
    let mut same_trigger = update(rs_command, &rust_team);
    same_trigger["trigger"] = serde_json::json!("mmrsupdated");
    same_trigger["description"] = serde_json::json!("edited twice");
    let (go_status, go_raw) = put(&http, GO, &token, &go_path, &{
        let mut b = update(go_command, &go_team);
        b["trigger"] = serde_json::json!("mmrsupdated");
        b["description"] = serde_json::json!("edited twice");
        b
    })
    .await;
    let (rs_status, rs_raw) = put(&http, RUST, &token, &rs_path, &same_trigger).await;
    assert_eq!(go_status, 200, "a command may keep its trigger: {go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    assert_eq!(parse(&rs_raw)["description"], "edited twice");

    // `IsValid` runs on the update too, inside the store and after `UpdateAt` is stamped.
    let mut bad_method = update(rs_command, &rust_team);
    bad_method["method"] = serde_json::json!("X");
    let (go_status, go_raw) = put(&http, GO, &token, &go_path, &{
        let mut b = update(go_command, &go_team);
        b["method"] = serde_json::json!("X");
        b
    })
    .await;
    let (rs_status, rs_raw) = put(&http, RUST, &token, &rs_path, &bad_method).await;
    assert_eq!(go_status, 400, "{go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    let body = assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rs_raw.as_bytes(),
        "an unknown method on update",
    );
    assert_eq!(body["id"], "model.command.is_valid.method.app_error");

    // `RequireCommandId` on each write — the **url**-param 400, before the body is even read.
    // `short` is alphanumeric, so Go's mux routes it and the id check is what refuses it.
    for (method, path) in [
        (reqwest::Method::PUT, "/api/v4/commands/short"),
        (reqwest::Method::DELETE, "/api/v4/commands/short"),
        (reqwest::Method::PUT, "/api/v4/commands/short/move"),
        (reqwest::Method::PUT, "/api/v4/commands/short/regen_token"),
    ] {
        let (go_status, go_raw) = send(&http, method.clone(), GO, &token, path, None).await;
        let (rs_status, rs_raw) = send(&http, method, RUST, &token, path, None).await;
        assert_eq!(go_status, 400, "{path}: {go_raw}");
        assert_eq!(rs_status, go_status, "{path}: {rs_raw}");
        let body =
            assert_error_bodies_match_except_known_gaps(go_raw.as_bytes(), rs_raw.as_bytes(), path);
        assert_eq!(
            body["id"], "api.context.invalid_url_param.app_error",
            "{path}"
        );
    }

    // **A body id that is not the path id is the decode error**, not a 404 about either.
    let mut wrong_id = update(rs_command, &rust_team);
    wrong_id["id"] = serde_json::json!("kh9x6ffcbir9uy8ta8m1yprkga");
    let (go_status, go_raw) = put(&http, GO, &token, &go_path, &wrong_id).await;
    let (rs_status, rs_raw) = put(&http, RUST, &token, &rs_path, &wrong_id).await;
    assert_eq!(go_status, 400, "{go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    let body = assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rs_raw.as_bytes(),
        "id mismatch",
    );
    assert_eq!(body["id"], "api.context.invalid_body_param.app_error");

    // **A different `team_id` is its own 400**, and so is an omitted one — this route does not
    // fill it in from the old command the way `updateIncomingHook` does.
    for (label, team) in [
        ("another team", other_team.as_str()),
        ("no team at all", ""),
    ] {
        let (go_status, go_raw) = put(&http, GO, &token, &go_path, &update(go_command, team)).await;
        let (rs_status, rs_raw) =
            put(&http, RUST, &token, &rs_path, &update(rs_command, team)).await;
        assert_eq!(go_status, 400, "{label}: {go_raw}");
        assert_eq!(rs_status, go_status, "{label}: {rs_raw}");
        let body = assert_error_bodies_match_except_known_gaps(
            go_raw.as_bytes(),
            rs_raw.as_bytes(),
            label,
        );
        assert_eq!(body["id"], "api.command.team_mismatch.app_error", "{label}");
    }

    // A command that does not exist: the 404 with the `save` id, from the fetch.
    let missing = "/api/v4/commands/zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let mut absent = update(rs_command, &rust_team);
    absent["id"] = serde_json::json!("zzzzzzzzzzzzzzzzzzzzzzzzzz");
    let (go_status, go_raw) = put(&http, GO, &token, missing, &absent).await;
    let (rs_status, rs_raw) = put(&http, RUST, &token, missing, &absent).await;
    assert_eq!(go_status, 404, "{go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    let body =
        assert_error_bodies_match_except_known_gaps(go_raw.as_bytes(), rs_raw.as_bytes(), missing);
    assert_eq!(body["id"], "store.sql_command.save.get.app_error");

    sweep(&[&go_team, &rust_team, &other_team]).await;
}

/// `deleteCommand` and `regenCommandToken` — the two routes whose bodies are not a `Command`.
#[tokio::test]
async fn delete_and_regen_token_answer_their_own_shapes() {
    if !stack_enabled() {
        return;
    }
    let _writes = COMMAND_WRITES.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let token = go_minted_token(&http).await;
    let go_team = create_team(&http, &token, "cwdelg").await;
    let rust_team = create_team(&http, &token, "cwdelr").await;

    let mut commands = Vec::new();
    for (base, team) in [(GO, &go_team), (RUST, &rust_team)] {
        for trigger in ["mmrsdel", "mmrsregen"] {
            let (status, raw) = post(
                &http,
                base,
                &token,
                "/api/v4/commands",
                &create_body(team, trigger),
            )
            .await;
            assert_eq!(status, 201, "{base}/{trigger}: {raw}");
            commands.push(parse(&raw));
        }
    }
    let id = |i: usize| commands[i]["id"].as_str().unwrap_or("").to_owned();

    // ---- regen_token ----
    let go_regen = format!("/api/v4/commands/{}/regen_token", id(1));
    let rs_regen = format!("/api/v4/commands/{}/regen_token", id(3));
    let (go_status, go_raw) = put_empty(&http, GO, &token, &go_regen).await;
    let (rs_status, rs_raw) = put_empty(&http, RUST, &token, &rs_regen).await;

    assert_eq!(go_status, 200, "{go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    // **`w.Write(MapToJSON(…))`, not the encoder** — so there is no trailing newline here, and
    // that is the one framing difference inside this module.
    assert!(
        !go_raw.ends_with('\n'),
        "Go writes the map directly: {go_raw:?}"
    );
    assert!(!rs_raw.ends_with('\n'), "and so must we: {rs_raw:?}");

    let go_body = parse(&go_raw);
    let rs_body = parse(&rs_raw);
    assert_eq!(
        go_body.as_object().map(serde_json::Map::len),
        Some(1),
        "one key, `token`, and not the whole command: {go_raw}"
    );
    assert_eq!(
        rs_body.as_object().map(serde_json::Map::len),
        go_body.as_object().map(serde_json::Map::len),
        "{rs_raw}"
    );
    assert_eq!(rs_body["token"].as_str().map(str::len), Some(26));
    assert_ne!(
        rs_body["token"], commands[3]["token"],
        "the token must actually change"
    );

    // The new token is the stored one, and `UpdateAt` moved — which nothing in
    // `App.RegenCommandToken` assigns; the store does.
    let (status, raw) = get(&http, RUST, &token, &format!("/api/v4/commands/{}", id(3))).await;
    assert_eq!(status, 200, "{raw}");
    let stored = parse(&raw);
    assert_eq!(stored["token"], rs_body["token"]);
    assert!(
        stored["update_at"].as_i64() > commands[3]["update_at"].as_i64(),
        "the store stamps UpdateAt on every update: {stored}"
    );
    assert_eq!(
        stored["trigger"], commands[3]["trigger"],
        "and nothing else moved"
    );

    // ---- delete ----
    let go_path = format!("/api/v4/commands/{}", id(0));
    let rs_path = format!("/api/v4/commands/{}", id(2));
    let (go_status, go_raw) = delete(&http, GO, &token, &go_path).await;
    let (rs_status, rs_raw) = delete(&http, RUST, &token, &rs_path).await;
    assert_eq!(go_status, 200, "{go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    assert_eq!(go_raw, r#"{"status":"OK"}"#, "ReturnStatusOK, unframed");
    assert_eq!(rs_raw, go_raw);

    // The soft delete hides it from `GetCommand`, so a second delete is the 404 — on both.
    let (go_status, go_raw) = delete(&http, GO, &token, &go_path).await;
    let (rs_status, rs_raw) = delete(&http, RUST, &token, &rs_path).await;
    assert_eq!(go_status, 404, "{go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    let body = assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rs_raw.as_bytes(),
        "double delete",
    );
    assert_eq!(body["id"], "store.sql_command.save.get.app_error");

    // **The deleted trigger is free again**: `GetByTeam` filters on `DeleteAt = 0`, so the
    // uniqueness check no longer sees it.
    let (status, raw) = post(
        &http,
        RUST,
        &token,
        "/api/v4/commands",
        &create_body(&rust_team, "mmrsdel"),
    )
    .await;
    assert_eq!(status, 201, "a deleted trigger is available again: {raw}");

    sweep(&[&go_team, &rust_team]).await;
}

/// `moveCommand`'s five gates, in the order that makes each one visible.
#[tokio::test]
async fn a_move_checks_the_destination_the_caller_and_the_creator() {
    if !stack_enabled() {
        return;
    }
    let _writes = COMMAND_WRITES.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let source = create_team(&http, &admin, "cwmvs").await;
    let go_dest = create_team(&http, &admin, "cwmvdg").await;
    let rs_dest = create_team(&http, &admin, "cwmvdr").await;

    let mut commands = Vec::new();
    for trigger in ["mmrsmoveg", "mmrsmover"] {
        let (status, raw) = post(
            &http,
            RUST,
            &admin,
            "/api/v4/commands",
            &create_body(&source, trigger),
        )
        .await;
        assert_eq!(status, 201, "{raw}");
        commands.push(parse(&raw));
    }
    let go_path = format!(
        "/api/v4/commands/{}/move",
        commands[0]["id"].as_str().unwrap_or("")
    );
    let rs_path = format!(
        "/api/v4/commands/{}/move",
        commands[1]["id"].as_str().unwrap_or("")
    );

    // **Gate 1 runs before the command is fetched**: a destination that does not exist is
    // `GetTeam`'s 404, even for a command id that is equally absent.
    let nowhere = serde_json::json!({ "team_id": "zzzzzzzzzzzzzzzzzzzzzzzzzz" });
    for path in [&go_path, &rs_path] {
        let (go_status, go_raw) = put(&http, GO, &admin, path, &nowhere).await;
        let (rs_status, rs_raw) = put(&http, RUST, &admin, path, &nowhere).await;
        assert_eq!(go_status, 404, "{go_raw}");
        assert_eq!(rs_status, go_status, "{rs_raw}");
        let body = assert_error_bodies_match_except_known_gaps(
            go_raw.as_bytes(),
            rs_raw.as_bytes(),
            "missing destination",
        );
        assert_eq!(
            body["id"], "app.team.get.find.app_error",
            "the team's 404, not the command's"
        );
    }

    // A body that is not a `CommandMoveRequest` — `SetInvalidParamWithErr("team_id")`, named for
    // the *field* rather than for the type, unlike every other write in this family.
    //
    // **That name is not on the wire.** `AppError.Params` is `json:"-"`; it survives only through
    // `Translate`, which turns the id into "Invalid or missing **team_id** in request body". Since
    // this port does not translate (D-092), the parameter name is unobservable in *our* body and
    // the only oracle for it is Go's own message — asserted here on Go alone, so the check still
    // fails if the handler ever names `command` instead.
    let mut go_message = String::new();
    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let response = http
            .put(format!("{base}{rs_path}"))
            .header("Authorization", format!("Bearer {admin}"))
            .header("Content-Type", "application/json")
            .body("not json")
            .send()
            .await
            .expect("a response");
        assert_eq!(response.status().as_u16(), 400, "{base}");
        let raw = response.text().await.expect("a body");
        if base == GO {
            go_message = parse(&raw)["message"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
        }
        bodies.push(raw);
    }
    assert!(
        go_message.contains("team_id") && !go_message.contains("command"),
        "Go names the parameter `team_id`: {go_message:?}"
    );
    let body = assert_error_bodies_match_except_known_gaps(
        bodies[0].as_bytes(),
        bodies[1].as_bytes(),
        "move body is not JSON",
    );
    assert_eq!(body["id"], "api.context.invalid_body_param.app_error");

    // **Gate 5: the creator's access to the destination, not the caller's.** The planted command
    // belongs to a plain `system_user`, who holds `manage_own_slash_commands` in no team at all,
    // so the admin — who passes every other gate — is refused with a 400.
    let stranger = create_plain_user(&http, &admin, &source, "cwmv").await;
    let Some(go_theirs) = plant_command("mvg", &source, &stranger.id).await else {
        return; // no DATABASE_URL
    };
    let Some(rs_theirs) = plant_command("mvr", &source, &stranger.id).await else {
        return;
    };
    let (go_status, go_raw) = put(
        &http,
        GO,
        &admin,
        &format!("/api/v4/commands/{go_theirs}/move"),
        &serde_json::json!({ "team_id": go_dest }),
    )
    .await;
    let (rs_status, rs_raw) = put(
        &http,
        RUST,
        &admin,
        &format!("/api/v4/commands/{rs_theirs}/move"),
        &serde_json::json!({ "team_id": rs_dest }),
    )
    .await;
    assert_eq!(
        go_status, 400,
        "the creator cannot reach the team: {go_raw}"
    );
    assert_eq!(rs_status, go_status, "{rs_raw}");
    let body = assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rs_raw.as_bytes(),
        "creator has no access",
    );
    assert_eq!(
        body["id"],
        "api.command.move_command.creator_no_permission.app_error"
    );

    // **The uniqueness check runs against the destination team, not the source.** A command
    // whose trigger is free where it lives and taken where it is going is refused — and a port
    // that checked the source team instead would allow every move in this test's happy path and
    // fail only here.
    for (base, dest, tag) in [(GO, &go_dest, "colg"), (RUST, &rs_dest, "colr")] {
        let trigger = format!("mmrscol{tag}");
        for team in [&source, dest] {
            let (status, raw) = post(
                &http,
                base,
                &admin,
                "/api/v4/commands",
                &create_body(team, &trigger),
            )
            .await;
            assert_eq!(status, 201, "{base}/{trigger} in {team}: {raw}");
        }
    }
    let Some(go_col) = occupied(&source, "mmrscolg").await else {
        return;
    };
    let Some(rs_col) = occupied(&source, "mmrscolr").await else {
        return;
    };
    let (go_status, go_raw) = put(
        &http,
        GO,
        &admin,
        &format!("/api/v4/commands/{go_col}/move"),
        &serde_json::json!({ "team_id": go_dest }),
    )
    .await;
    let (rs_status, rs_raw) = put(
        &http,
        RUST,
        &admin,
        &format!("/api/v4/commands/{rs_col}/move"),
        &serde_json::json!({ "team_id": rs_dest }),
    )
    .await;
    assert_eq!(go_status, 400, "the destination already has it: {go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    let body = assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rs_raw.as_bytes(),
        "move onto a taken trigger",
    );
    assert_eq!(body["id"], "api.command.duplicate_trigger.app_error");

    // The happy path: `{"status":"OK"}`, and the row's `team_id` really moved.
    let (go_status, go_raw) = put(
        &http,
        GO,
        &admin,
        &go_path,
        &serde_json::json!({ "team_id": go_dest }),
    )
    .await;
    let (rs_status, rs_raw) = put(
        &http,
        RUST,
        &admin,
        &rs_path,
        &serde_json::json!({ "team_id": rs_dest }),
    )
    .await;
    assert_eq!(go_status, 200, "{go_raw}");
    assert_eq!(rs_status, go_status, "{rs_raw}");
    assert_eq!(go_raw, r#"{"status":"OK"}"#);
    assert_eq!(rs_raw, go_raw);

    let (status, raw) = get(
        &http,
        RUST,
        &admin,
        &format!(
            "/api/v4/commands/{}",
            commands[1]["id"].as_str().unwrap_or("")
        ),
    )
    .await;
    assert_eq!(status, 200, "{raw}");
    let moved = parse(&raw);
    assert_eq!(moved["team_id"], serde_json::json!(rs_dest));
    assert_eq!(moved["trigger"], "mmrsmover", "only the team moved");
    assert_eq!(moved["token"], commands[1]["token"]);
    assert!(
        moved["update_at"].as_i64() > commands[1]["update_at"].as_i64(),
        "the store stamps UpdateAt here too: {moved}"
    );

    // And the trigger is now free in the source team, taken in the destination.
    let (status, raw) = post(
        &http,
        RUST,
        &admin,
        "/api/v4/commands",
        &create_body(&source, "mmrsmover"),
    )
    .await;
    assert_eq!(status, 201, "the source team no longer holds it: {raw}");

    sweep(&[&source, &go_dest, &rs_dest]).await;
    common::delete_plain_user(&http, &admin, &stranger.id).await;
}

/// **The ownership ladder is a 404 then a 403, and the writes disagree with `getCommand` about
/// the second rung.**
///
/// Three callers against the same command, one failing each rung, on all four write routes.
#[tokio::test]
async fn the_ownership_ladder_answers_404_then_403_on_every_write() {
    if !stack_enabled() {
        return;
    }
    let _writes = COMMAND_WRITES.lock().await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "cwlad").await;

    let Some(author_role) = common::plant_role("cwauthor", "manage_own_slash_commands").await
    else {
        return; // no DATABASE_URL
    };

    // Holds `manage_own_slash_commands` and not `manage_others`: passes rung one, fails rung two
    // on a command someone else created.
    let author = create_plain_user(&http, &admin, &team, "cwauthor").await;
    common::set_user_roles(&author.id, &format!("system_user {author_role}")).await;
    let author_token = common::login_plain_user(&http, "cwauthor").await;

    // Holds neither: fails rung one, and so is told the command does not exist.
    let plain = create_plain_user(&http, &admin, &team, "cwplain").await;

    // Four commands owned by the admin, one per route, so each refusal is tested against a
    // command that is still there.
    let mut ids = Vec::new();
    for tag in ["lad1", "lad2", "lad3", "lad4"] {
        let Some(id) = plant_command(tag, &team, logged_in_user_id()).await else {
            return;
        };
        ids.push(id);
    }

    let update_body = |id: &str| {
        let mut b = create_body(&team, "mmrsladder");
        b["id"] = serde_json::json!(id);
        b
    };

    for (label, token, expected) in [
        ("no manage_own at all", &plain.token, 404u16),
        ("manage_own but not the creator", &author_token, 403u16),
    ] {
        let cases: Vec<(&str, String, Option<serde_json::Value>, reqwest::Method)> = vec![
            (
                "update",
                format!("/api/v4/commands/{}", ids[0]),
                Some(update_body(&ids[0])),
                reqwest::Method::PUT,
            ),
            (
                "delete",
                format!("/api/v4/commands/{}", ids[1]),
                None,
                reqwest::Method::DELETE,
            ),
            (
                "move",
                format!("/api/v4/commands/{}/move", ids[2]),
                Some(serde_json::json!({ "team_id": team })),
                reqwest::Method::PUT,
            ),
            (
                "regen_token",
                format!("/api/v4/commands/{}/regen_token", ids[3]),
                None,
                reqwest::Method::PUT,
            ),
        ];

        for (route, path, request_body, method) in cases {
            // **`move` is the exception, measured not assumed.** Its `manage_own` check on the
            // *destination* team runs before `GetCommand`, so the caller who fails rung one never
            // reaches the 404 the other three give — they are refused at gate 2 with a plain 403.
            // The first run of this test expected 404 here and Go said 403.
            let expected = if route == "move" { 403 } else { expected };
            let (go_status, go_raw) = send(
                &http,
                method.clone(),
                GO,
                token,
                &path,
                request_body.as_ref(),
            )
            .await;
            let (rs_status, rs_raw) =
                send(&http, method, RUST, token, &path, request_body.as_ref()).await;
            assert_eq!(
                go_status, expected,
                "{route} / {label}: Go answered {go_status}: {go_raw}"
            );
            assert_eq!(
                rs_status, go_status,
                "{route} / {label}: Go {go_status}, us {rs_status}: {rs_raw}"
            );
            let body = assert_error_bodies_match_except_known_gaps(
                go_raw.as_bytes(),
                rs_raw.as_bytes(),
                route,
            );
            let expected_id = if expected == 404 {
                "store.sql_command.save.get.app_error"
            } else {
                "api.context.permissions.app_error"
            };
            assert_eq!(body["id"], expected_id, "{route} / {label}");
        }
    }

    // **The contrast that makes the 403 above about the creator**: `getCommand`, same caller,
    // same command, answers **404** where the writes answer 403.
    let (go_status, go_raw) = get(
        &http,
        GO,
        &author_token,
        &format!("/api/v4/commands/{}", ids[0]),
    )
    .await;
    let (rs_status, rs_raw) = get(
        &http,
        RUST,
        &author_token,
        &format!("/api/v4/commands/{}", ids[0]),
    )
    .await;
    assert_eq!(
        go_status, 404,
        "the read hides what the writes admit: {go_raw}"
    );
    assert_eq!(rs_status, go_status, "{rs_raw}");
    assert_error_bodies_match_except_known_gaps(go_raw.as_bytes(), rs_raw.as_bytes(), "read");

    // And the positive control: the author on **their own** command passes rung two.
    let Some(theirs) = plant_command("lad5", &team, &author.id).await else {
        return;
    };
    let (status, raw) = put_empty(
        &http,
        RUST,
        &author_token,
        &format!("/api/v4/commands/{theirs}/regen_token"),
    )
    .await;
    assert_eq!(status, 200, "the creator may regenerate their token: {raw}");

    sweep(&[&team]).await;
    for id in [&author.id, &plain.id] {
        common::delete_plain_user(&http, &admin, id).await;
    }
}

/// **`POST /api/v4/commands/execute` must still reach Go.** It shares a path pattern with
/// `{command_id}`, on which this module registers `PUT`, `DELETE` and `GET` but deliberately not
/// `POST` — so the method fallback forwards it. Registering `create_command` there instead of on
/// `/api/v4/commands` would swallow the execute route silently.
#[tokio::test]
async fn the_execute_route_is_still_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _writes = COMMAND_WRITES.lock().await;
    let http = client();
    let token = go_minted_token(&http).await;

    let response = http
        .post(format!("{RUST}/api/v4/commands/execute"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "command": "/away", "channel_id": "" }))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "executeCommand is not ported and must be forwarded"
    );
}
