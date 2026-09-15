//! Cross-server parity for the three slash-command routes that reach the built-in registry:
//! `POST /api/v4/commands/execute`, `GET /api/v4/teams/{team_id}/commands/autocomplete` and
//! `GET /api/v4/teams/{team_id}/commands/autocomplete_suggestions`.
//!
//! ```sh
//! scripts/parity.sh --test parity command_dispatch
//! ```
//!
//! # The fixture
//!
//! Each test makes its own team and plants its own `Commands` rows (ids `mmrsdisp<tag>…`), so
//! nothing here lists another suite's commands. The planted rows are chosen so that the list's
//! precedence rules each change the answer: one autocompleting custom command (listed,
//! sanitised — its token and URL are blank in the answer), one that does not autocomplete (absent),
//! and a custom `/shrug` (which hides the built-in one). The custom command's description carries
//! `&` so `encoding/json`'s HTML escaping is visible in the bytes.
//!
//! # The list is compared as a set
//!
//! Go ranges over a map for the built-ins, so its order changes between two reads of the same
//! server (measured). Every element is compared exactly; only the order is not.
//!
//! # What is asserted forwarded
//!
//! A non-English `Accept-Language`, a suggestion input that reaches a dynamic-list argument, and
//! every execute that would run something: a built-in and a custom command.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_plain_user, create_team, delete_plain_user, fixture_pool, go_minted_token,
    logged_in_user_id, stack_enabled,
};

/// Plant one `Commands` row for `team_id`. The id is `mmrsdisp`, the tag and the index, padded
/// with zeros to 26 characters.
async fn plant(tag: &str, n: u32, team_id: &str, trigger: &str, auto_complete: bool, url: &str) {
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let id = format!("{:0<26}", format!("mmrsdisp{tag}{n}"));
    sqlx::query(
        r#"
        INSERT INTO commands (id, token, createat, updateat, deleteat, creatorid, teamid,
                              "trigger", method, username, iconurl, autocomplete,
                              autocompletedesc, autocompletehint, displayname, description, url,
                              pluginid)
        VALUES ($1, 'mmrsdisptoken0000000000000', 1788600000000, 1788600000000, 0, $2, $3, $4,
                'P', 'someone', 'http://icon.invalid/a.png', $5, 'planted & described',
                '[planted <hint>]', $4, 'a description', $6, '')
        "#,
    )
    .bind(&id)
    .bind(logged_in_user_id())
    .bind(team_id)
    .bind(trigger)
    .bind(auto_complete)
    .bind(url)
    .execute(&pool)
    .await
    .expect("the command row is written");
}

async fn unplant(tag: &str) {
    let Some(pool) = fixture_pool().await else {
        return;
    };
    sqlx::query("DELETE FROM commands WHERE id LIKE $1")
        .bind(format!("mmrsdisp{tag}%"))
        .execute(&pool)
        .await
        .expect("the planted commands are removed");
}

/// One autocomplete refusal: the path, the token, the query and the status both servers give.
type RefusalCase<'a> = (String, &'a str, Vec<(&'a str, &'a str)>, u16);

/// `(status, body, x-mmrs-served-by)` for one request.
async fn send(
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    query: &[(&str, &str)],
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> (u16, Vec<u8>, Option<String>) {
    let mut request = client()
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .query(query);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    if let Some(body) = body {
        request = request
            .header("Content-Type", "application/json")
            .body(body.to_owned());
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("reads").to_vec(),
        served,
    )
}

/// A JSON array of commands, sorted by trigger.
fn by_trigger(raw: &[u8]) -> Vec<serde_json::Value> {
    let mut commands: Vec<serde_json::Value> = serde_json::from_slice::<serde_json::Value>(raw)
        .expect("a JSON array")
        .as_array()
        .expect("an array")
        .clone();
    commands.sort_by(|a, b| a["trigger"].as_str().cmp(&b["trigger"].as_str()));
    commands
}

/// The list, as a set, with a custom command, a hidden one and a shadowed built-in; and it
/// forwards for a request whose translate function is not English.
#[tokio::test]
async fn the_autocomplete_list_matches_go_as_a_set() {
    if !stack_enabled() {
        return;
    }
    let token = go_minted_token(&client()).await;
    let team = create_team(&client(), &token, "cmdlist").await;
    unplant("l").await;
    plant(
        "l",
        1,
        &team,
        "mmrsdispyes",
        true,
        "https://example.invalid/?a=1&b=2",
    )
    .await;
    plant(
        "l",
        2,
        &team,
        "mmrsdispno",
        false,
        "https://example.invalid/",
    )
    .await;
    plant("l", 3, &team, "shrug", true, "https://example.invalid/").await;

    let path = format!("/api/v4/teams/{team}/commands/autocomplete");
    let (go_status, go_body, _) =
        send(GO, reqwest::Method::GET, &path, &token, &[], &[], None).await;
    let (rs_status, rs_body, served) =
        send(RUST, reqwest::Method::GET, &path, &token, &[], &[], None).await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body.last(), Some(&b'\n'));
    assert_eq!(rs_body.last(), Some(&b'\n'), "json.NewEncoder's newline");

    let (go, rs) = (by_trigger(&go_body), by_trigger(&rs_body));
    assert_eq!(go, rs, "the command sets differ");

    let triggers: Vec<&str> = rs.iter().filter_map(|c| c["trigger"].as_str()).collect();
    assert!(triggers.contains(&"mmrsdispyes"));
    assert!(
        !triggers.contains(&"mmrsdispno"),
        "a non-autocompleting command is listed"
    );
    assert_eq!(triggers.iter().filter(|t| **t == "shrug").count(), 1);
    let shrug = rs.iter().find(|c| c["trigger"] == "shrug").expect("shrug");
    assert_eq!(
        shrug["auto_complete_desc"], "planted & described",
        "the custom shrug wins"
    );
    assert_eq!(shrug["token"], "", "custom commands are sanitised");
    assert_eq!(shrug["url"], "");
    assert_eq!(
        go_body.windows(6).any(|w| w == b"\\u0026"),
        rs_body.windows(6).any(|w| w == b"\\u0026"),
        "encoding/json's HTML escaping"
    );

    for (language, expected) in [("de", "go"), ("en-AU", "go"), ("en-US,de;q=0.5", "rust")] {
        let (_, _, served) = send(
            RUST,
            reqwest::Method::GET,
            &path,
            &token,
            &[],
            &[("Accept-Language", language)],
            None,
        )
        .await;
        assert_eq!(
            served.as_deref(),
            Some(expected),
            "Accept-Language: {language}"
        );
    }

    unplant("l").await;
}

/// The two refusals the list and the suggestions share, plus the suggestions' own.
#[tokio::test]
async fn the_autocomplete_refusals_match() {
    if !stack_enabled() {
        return;
    }
    let admin = go_minted_token(&client()).await;
    let team = create_team(&client(), &admin, "cmdref").await;
    let other = create_team(&client(), &admin, "cmdrefo").await;
    let outsider = create_plain_user(&client(), &admin, &other, "cmdref").await;

    let cases: Vec<RefusalCase<'_>> = vec![
        (
            format!("/api/v4/teams/{team}/commands/autocomplete"),
            outsider.token.as_str(),
            vec![],
            403,
        ),
        (
            format!("/api/v4/teams/{team}/commands/autocomplete_suggestions"),
            outsider.token.as_str(),
            vec![("user_input", "/a")],
            403,
        ),
        (
            "/api/v4/teams/zzzz/commands/autocomplete".to_owned(),
            admin.as_str(),
            vec![],
            400,
        ),
        (
            "/api/v4/teams/zzzz/commands/autocomplete_suggestions".to_owned(),
            admin.as_str(),
            vec![("user_input", "/a")],
            400,
        ),
        (
            format!("/api/v4/teams/{team}/commands/autocomplete_suggestions"),
            admin.as_str(),
            vec![],
            400,
        ),
        (
            format!("/api/v4/teams/{team}/commands/autocomplete_suggestions"),
            admin.as_str(),
            vec![("user_input", "")],
            400,
        ),
    ];
    for (path, token, query, expected) in cases {
        let (go_status, go_body, _) =
            send(GO, reqwest::Method::GET, &path, token, &query, &[], None).await;
        let (rs_status, rs_body, served) =
            send(RUST, reqwest::Method::GET, &path, token, &query, &[], None).await;
        let context = format!("{path} {query:?}");
        assert_eq!(served.as_deref(), Some("rust"), "{context}");
        assert_eq!((go_status, rs_status), (expected, expected), "{context}");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
    }

    delete_plain_user(&client(), &admin, &outsider.id).await;
}

/// Suggestions byte for byte, for an admin and for a plain member, over every parser branch the
/// built-ins reach — subcommands, named arguments offered, completed and consumed, positional
/// and quoted text — and a custom command. The dynamic-list input is Go's.
#[tokio::test]
async fn the_suggestions_match_byte_for_byte() {
    if !stack_enabled() {
        return;
    }
    let admin = go_minted_token(&client()).await;
    let team = create_team(&client(), &admin, "cmdsug").await;
    let member = create_plain_user(&client(), &admin, &team, "cmdsug").await;
    unplant("s").await;
    plant(
        "s",
        1,
        &team,
        "mmrsdispsug",
        true,
        "https://example.invalid/",
    )
    .await;

    let path = format!("/api/v4/teams/{team}/commands/autocomplete_suggestions");
    let served_inputs = [
        "/",
        "/sec",
        "/SEC",
        "/secure-connection ",
        "/secure-connection cr",
        "/secure-connection create ",
        "/secure-connection create --na",
        // A named argument typed exactly, with its one trailing space: the parser turns the
        // leftover " " into "" before the text argument, and the completion shows which.
        "/secure-connection create --name ",
        "/secure-connection create --name bob ",
        "/secure-connection create --name bob --displayname ",
        "/secure-connection remove ",
        "/rename ",
        "/rename \"ab",
        "/rename \"a b\" ",
        "/rename word ",
        "/share-channel ",
        "/ECHO",
        "/mmrs",
        "/nothing matches",
        "mmrsdispsug",
    ];
    for token in [admin.as_str(), member.token.as_str()] {
        for input in served_inputs {
            let query = [("user_input", input)];
            let (go_status, go_body, _) =
                send(GO, reqwest::Method::GET, &path, token, &query, &[], None).await;
            let (rs_status, rs_body, served) =
                send(RUST, reqwest::Method::GET, &path, token, &query, &[], None).await;
            assert_eq!(served.as_deref(), Some("rust"), "{input:?}");
            assert_eq!((go_status, rs_status), (200, 200), "{input:?}");
            assert_eq!(
                String::from_utf8_lossy(&go_body),
                String::from_utf8_lossy(&rs_body),
                "{input:?}"
            );
        }
    }

    for input in [
        "/secure-connection remove --connectionID ",
        "/share-channel invite --connectionID ",
    ] {
        let (_, _, served) = send(
            RUST,
            reqwest::Method::GET,
            &path,
            &admin,
            &[("user_input", input)],
            &[],
            None,
        )
        .await;
        assert_eq!(
            served.as_deref(),
            Some("go"),
            "{input:?} reaches a dynamic list"
        );
    }

    unplant("s").await;
    delete_plain_user(&client(), &admin, &member.id).await;
}

/// Every refusal of `executeCommand` and every dispatch outcome that is not a run.
#[tokio::test]
async fn the_execute_refusals_and_misses_match() {
    if !stack_enabled() {
        return;
    }
    let admin = go_minted_token(&client()).await;
    let me = logged_in_user_id();
    let team = create_team(&client(), &admin, "cmdexe").await;
    let channel = create_channel(&client(), &admin, &team, "cmdexe").await;
    let archived = create_channel(&client(), &admin, &team, "cmdexea").await;
    common::delete_channel(&client(), &admin, &archived).await;
    let plain = create_plain_user(&client(), &admin, &team, "cmdexe").await;

    let direct = async |token: &str, a: &str, b: &str| -> String {
        let (status, body, _) = send(
            GO,
            reqwest::Method::POST,
            "/api/v4/channels/direct",
            token,
            &[],
            &[],
            Some(&serde_json::json!([a, b]).to_string()),
        )
        .await;
        assert!(
            status == 200 || status == 201,
            "{}",
            String::from_utf8_lossy(&body)
        );
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let admin_dm = direct(&admin, me, me).await;
    let plain_dm = direct(&plain.token, &plain.id, &plain.id).await;

    let body = |command: &str, channel_id: &str, team_id: &str| {
        serde_json::json!({"command": command, "channel_id": channel_id, "team_id": team_id})
            .to_string()
    };
    let nowhere = "zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let cases: Vec<(&str, &str, String, u16)> = vec![
        (
            "a body that does not decode",
            &admin,
            "{not json".to_owned(),
            400,
        ),
        ("a null body", &admin, "null".to_owned(), 400),
        (
            "a mistyped field",
            &admin,
            r#"{"command":5}"#.to_owned(),
            400,
        ),
        ("a bare slash", &admin, body("/", &channel, ""), 400),
        ("no slash", &admin, body("shrug", &channel, ""), 400),
        (
            "a short channel id",
            &admin,
            body("/shrug", "short", ""),
            400,
        ),
        ("no such channel", &admin, body("/shrug", nowhere, ""), 403),
        (
            "an archived channel",
            &admin,
            body("/shrug", &archived, ""),
            400,
        ),
        (
            "an unknown trigger",
            &admin,
            body("/mmrsnosuchcommand hello", &channel, ""),
            404,
        ),
        (
            "an upper-case unknown trigger",
            &admin,
            body("/MMRSNOPE", &channel, nowhere),
            404,
        ),
        (
            "a nil provider",
            &admin,
            body("/exportlink", &channel, ""),
            404,
        ),
        (
            "a DM naming no team",
            &admin,
            body("/shrug", &admin_dm, nowhere),
            404,
        ),
        (
            "a DM naming a real team",
            &admin,
            body("/mmrsnope", &admin_dm, &team),
            404,
        ),
        (
            "a DM outside any team",
            &plain.token,
            body("/shrug", &plain_dm, nowhere),
            403,
        ),
    ];
    for (label, token, request, expected) in cases {
        let (go_status, go_body, _) = send(
            GO,
            reqwest::Method::POST,
            "/api/v4/commands/execute",
            token,
            &[],
            &[],
            Some(&request),
        )
        .await;
        let (rs_status, rs_body, served) = send(
            RUST,
            reqwest::Method::POST,
            "/api/v4/commands/execute",
            token,
            &[],
            &[],
            Some(&request),
        )
        .await;
        assert_eq!(served.as_deref(), Some("rust"), "{label}");
        assert_eq!((go_status, rs_status), (expected, expected), "{label}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label);
        eprintln!("{label}: {}", go["id"]);
    }

    delete_plain_user(&client(), &admin, &plain.id).await;
}

/// Anything that would run is Go's: a built-in, and a custom command of the team.
#[tokio::test]
async fn a_command_that_would_run_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let admin = go_minted_token(&client()).await;
    let team = create_team(&client(), &admin, "cmdrun").await;
    let channel = create_channel(&client(), &admin, &team, "cmdrun").await;
    unplant("r").await;
    plant("r", 1, &team, "mmrsdisprun", true, "http://127.0.0.1:1/").await;

    for command in ["/shrug from the parity suite", "/mmrsdisprun now"] {
        let request = serde_json::json!({"command": command, "channel_id": channel}).to_string();
        let (_, _, served) = send(
            RUST,
            reqwest::Method::POST,
            "/api/v4/commands/execute",
            &admin,
            &[],
            &[],
            Some(&request),
        )
        .await;
        assert_eq!(served.as_deref(), Some("go"), "{command}");
    }

    unplant("r").await;
}
