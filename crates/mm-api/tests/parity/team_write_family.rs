//! Cross-server parity for the team write family: `createTeam`, `updateTeamPrivacy` and
//! `searchTeams`.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity team_write_family
//! ```
//!
//! # Every team this file touches it created
//!
//! Privacy changes and team deletions are shared state that a dozen other suites read. Nothing
//! here writes to the seeded fixture team; each test creates its own and leaves it alive, per
//! [D-155]'s note that Go's `town-square`/`off-topic` are orphaned rather than deleted.

use crate::common;

use common::{GO, RUST, client, create_team, go_minted_token, stack_enabled};

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
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

/// Send the same raw bytes to both servers and assert status **and** body agree.
async fn both_raw(
    http: &reqwest::Client,
    method: reqwest::Method,
    token: &str,
    path: &str,
    body: &'static str,
) -> (u16, String) {
    let mut results = Vec::new();
    for base in [GO, RUST] {
        let response = http
            .request(method.clone(), format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
        let status = response.status().as_u16();
        if base == RUST {
            common::assert_served_by_rust(response.headers(), path);
        }
        results.push((status, response.text().await.expect("a body")));
    }
    assert_eq!(
        results[0].0, results[1].0,
        "status for body {body}: {results:?}"
    );
    (results[0].0, results[0].1.clone())
}

fn error_id(raw: &str) -> String {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("<not an AppError: {raw}>"))
}

// ---------------------------------------------------------------------------------------------
// updateTeamPrivacy
// ---------------------------------------------------------------------------------------------

/// `O` → `I` mints a **new invite id**; `I` → `O` keeps it. The two servers are asked the same
/// question on two teams of their own, and what is compared is the *relationship* between the
/// before and after values, not the ids themselves, which cannot match across two teams.
#[tokio::test]
async fn closing_a_team_regenerates_the_invite_id_and_reopening_does_not() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    for base in [GO, RUST] {
        let tag = if base == GO { "twfprvg" } else { "twfprvr" };
        let team_id = create_team(&http, &token, tag).await;

        let (_status, raw) = send(
            &http,
            reqwest::Method::GET,
            base,
            &token,
            &format!("/api/v4/teams/{team_id}"),
            None,
        )
        .await;
        let before: serde_json::Value = serde_json::from_str(&raw).expect("a team");
        let first_invite = before["invite_id"]
            .as_str()
            .expect("an invite id")
            .to_owned();
        assert_eq!(before["type"], "O", "the fixture team starts open");

        // O -> I: the type and the flag both move, and the invite id is replaced.
        let (status, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &token,
            &format!("/api/v4/teams/{team_id}/privacy"),
            Some(&serde_json::json!({"privacy": "I"})),
        )
        .await;
        assert_eq!(status, 200, "{base}: closing the team: {raw}");
        let closed: serde_json::Value = serde_json::from_str(&raw).expect("a team");
        assert_eq!(closed["type"], "I", "{base}");
        assert_eq!(closed["allow_open_invite"], false, "{base}");
        let second_invite = closed["invite_id"]
            .as_str()
            .expect("an invite id")
            .to_owned();
        assert_ne!(
            first_invite, second_invite,
            "{base}: closing a team must invalidate every invite link already handed out"
        );

        // A no-op I -> I changes nothing, so the left half of the predicate is false.
        let (status, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &token,
            &format!("/api/v4/teams/{team_id}/privacy"),
            Some(&serde_json::json!({"privacy": "I"})),
        )
        .await;
        assert_eq!(status, 200, "{base}: the no-op write: {raw}");
        let again: serde_json::Value = serde_json::from_str(&raw).expect("a team");
        assert_eq!(
            again["invite_id"].as_str(),
            Some(second_invite.as_str()),
            "{base}: a no-op privacy write must not mint a new invite id"
        );

        // I -> O keeps it.
        let (status, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &token,
            &format!("/api/v4/teams/{team_id}/privacy"),
            Some(&serde_json::json!({"privacy": "O"})),
        )
        .await;
        assert_eq!(status, 200, "{base}: reopening the team: {raw}");
        let opened: serde_json::Value = serde_json::from_str(&raw).expect("a team");
        assert_eq!(opened["type"], "O", "{base}");
        assert_eq!(opened["allow_open_invite"], true, "{base}");
        assert_eq!(
            opened["invite_id"].as_str(),
            Some(second_invite.as_str()),
            "{base}: opening a team keeps its invite id"
        );
    }
}

/// Every unusable body is the **same** 400 naming `privacy` — there is no distinct
/// malformed-body answer, because `StringInterfaceFromJSON` swallows the decode error.
#[tokio::test]
async fn every_unusable_privacy_body_is_one_400() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let team_id = create_team(&http, &token, "twfbad").await;
    let path = format!("/api/v4/teams/{team_id}/privacy");

    for body in [
        "",
        "not json",
        "[]",
        "null",
        "{}",
        r#"{"privacy":null}"#,
        r#"{"privacy":1}"#,
        r#"{"privacy":"P"}"#,
        r#"{"privacy":"o"}"#,
        r#"{"Privacy":"O"}"#,
    ] {
        let (status, raw) = both_raw(&http, reqwest::Method::PUT, &token, &path, body).await;
        assert_eq!(status, 400, "body {body:?}: {raw}");
        assert_eq!(
            error_id(&raw),
            "api.context.invalid_body_param.app_error",
            "body {body:?}"
        );
    }

    // `json.Decoder.Decode` reads one value and stops, so trailing junk is accepted.
    let (status, raw) = both_raw(
        &http,
        reqwest::Method::PUT,
        &token,
        &path,
        r#"{"privacy":"O"} and then some"#,
    )
    .await;
    assert_eq!(status, 200, "trailing junk after the first value: {raw}");
}

// ---------------------------------------------------------------------------------------------
// searchTeams
// ---------------------------------------------------------------------------------------------

/// Both servers, one admin token, one term that can only match the team this test made. The
/// bodies are compared byte for byte after the two teams' own ids are normalised away.
#[tokio::test]
async fn the_search_answers_the_same_teams_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let team_id = create_team(&http, &token, "twfsrch").await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::POST,
            base,
            &token,
            "/api/v4/teams/search",
            Some(&serde_json::json!({"term": "mmrs-parity-twfsrch"})),
        )
        .await;
        assert_eq!(status, 200, "{base}: {raw}");
        assert!(
            !raw.ends_with('\n'),
            "{base}: `w.Write` leaves no trailing newline"
        );
        let teams: serde_json::Value = serde_json::from_str(&raw).expect("an array");
        assert_eq!(
            teams.as_array().map(Vec::len),
            Some(1),
            "{base}: exactly the team this test made: {raw}"
        );
        assert_eq!(teams[0]["id"].as_str(), Some(team_id.as_str()), "{base}");
        bodies.push(raw);
    }
    assert_eq!(bodies[0], bodies[1], "byte for byte");
}

/// `page` **and** `per_page` switch the shape to `{"teams": [...], "total_count": N}`; either
/// alone leaves a bare array. `total_count` counts the whole match, not the page.
#[tokio::test]
async fn the_response_shape_needs_both_pagination_fields() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    // Three teams sharing a prefix, so a per_page of 2 genuinely truncates.
    for tag in ["twfpg1", "twfpg2", "twfpg3"] {
        create_team(&http, &token, tag).await;
    }

    for (body, shaped) in [
        (serde_json::json!({"term": "mmrs-parity-twfpg"}), false),
        (
            serde_json::json!({"term": "mmrs-parity-twfpg", "page": 0}),
            false,
        ),
        (
            serde_json::json!({"term": "mmrs-parity-twfpg", "per_page": 2}),
            false,
        ),
        (
            serde_json::json!({"term": "mmrs-parity-twfpg", "page": 0, "per_page": 2}),
            true,
        ),
    ] {
        let mut bodies = Vec::new();
        for base in [GO, RUST] {
            let (status, raw) = send(
                &http,
                reqwest::Method::POST,
                base,
                &token,
                "/api/v4/teams/search",
                Some(&body),
            )
            .await;
            assert_eq!(status, 200, "{base} for {body}: {raw}");
            let value: serde_json::Value = serde_json::from_str(&raw).expect("json");
            if shaped {
                assert_eq!(
                    value["total_count"].as_i64(),
                    Some(3),
                    "{base}: the count spans the match, not the page: {raw}"
                );
                assert_eq!(
                    value["teams"].as_array().map(Vec::len),
                    Some(2),
                    "{base}: the page is truncated: {raw}"
                );
            } else {
                assert_eq!(
                    value.as_array().map(Vec::len),
                    Some(3),
                    "{base}: a bare array of every match for {body}: {raw}"
                );
            }
            bodies.push(raw);
        }
        assert_eq!(bodies[0], bodies[1], "byte for byte for {body}");
    }
}

/// The **public-only** arm: an ordinary `system_user` holds `list_public_teams` and not
/// `list_private_teams`, which is the branch with the two divergences a reader loses.
///
/// - A request carrying `page` **or** `per_page` — either alone — is a **501**, not a 400 and not
///   a silently unpaginated 200. The response *shape* further down needs both; this refusal needs
///   either, and no fixture that sends both fields together can see the difference.
/// - `exclude_policy_constrained` is checked for **presence**: `false` is refused exactly as
///   `true` is, with a 403 naming the retention-policy permission, and that refusal runs *before*
///   the pagination one.
/// - The search itself is narrowed to open teams, so a team this test closes disappears from it.
#[tokio::test]
async fn the_public_only_arm_refuses_pagination_and_hides_closed_teams() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let open_team = create_team(&http, &admin, "twfpubo").await;
    let closed_team = create_team(&http, &admin, "twfpubc").await;
    let plain = common::create_plain_user(&http, &admin, &open_team, "twfsearch").await;

    // **`createTeam` leaves `allow_open_invite` false even for a `type: "O"` team**, and
    // `SearchOpen` filters on the flag as well as the type — so the "open" team has to be opened
    // explicitly or the public-only arm finds nothing at all and this fixture proves nothing.
    for (team_id, privacy) in [(&open_team, "O"), (&closed_team, "I")] {
        let (status, raw) = send(
            &http,
            reqwest::Method::PUT,
            RUST,
            &admin,
            &format!("/api/v4/teams/{team_id}/privacy"),
            Some(&serde_json::json!({ "privacy": privacy })),
        )
        .await;
        assert_eq!(status, 200, "setting the fixture team to {privacy}: {raw}");
    }

    // Either pagination field alone is a 501 with the public-arm id.
    for body in [
        serde_json::json!({"term": "mmrs-parity-twfpub", "page": 0}),
        serde_json::json!({"term": "mmrs-parity-twfpub", "per_page": 5}),
        serde_json::json!({"term": "mmrs-parity-twfpub", "page": 0, "per_page": 5}),
    ] {
        let mut answers = Vec::new();
        for base in [GO, RUST] {
            let (status, raw) = send(
                &http,
                reqwest::Method::POST,
                base,
                &plain.token,
                "/api/v4/teams/search",
                Some(&body),
            )
            .await;
            assert_eq!(status, 501, "{base} for {body}: {raw}");
            answers.push(error_id(&raw));
        }
        assert_eq!(
            answers[0], "api.team.search_teams.pagination_not_implemented.public_team_search",
            "the public arm has its own id"
        );
        assert_eq!(answers[0], answers[1]);
    }

    // The retention 403 runs **before** the 501, so a body carrying both gets the 403.
    for value in [serde_json::json!(true), serde_json::json!(false)] {
        let mut answers = Vec::new();
        for base in [GO, RUST] {
            let (status, raw) = send(
                &http,
                reqwest::Method::POST,
                base,
                &plain.token,
                "/api/v4/teams/search",
                Some(&serde_json::json!({
                    "term": "mmrs-parity-twfpub",
                    "exclude_policy_constrained": value,
                    "page": 0,
                })),
            )
            .await;
            assert_eq!(
                status, 403,
                "{base}: presence, not truth ({value}), and it precedes the 501: {raw}"
            );
            answers.push(error_id(&raw));
        }
        assert_eq!(answers[0], answers[1], "the same refusal id");
    }

    // And the unpaginated search sees the open team and not the closed one.
    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::POST,
            base,
            &plain.token,
            "/api/v4/teams/search",
            Some(&serde_json::json!({"term": "mmrs-parity-twfpub"})),
        )
        .await;
        assert_eq!(status, 200, "{base}: {raw}");
        let teams: serde_json::Value = serde_json::from_str(&raw).expect("an array");
        let ids: Vec<&str> = teams
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(|t| t["id"].as_str())
            .collect();
        assert_eq!(ids, vec![open_team.as_str()], "{base}: {raw}");
        bodies.push(raw);
    }
    assert_eq!(bodies[0], bodies[1], "byte for byte");

    // The admin, holding both list permissions, still sees both teams.
    let (status, raw) = send(
        &http,
        reqwest::Method::POST,
        RUST,
        &admin,
        "/api/v4/teams/search",
        Some(&serde_json::json!({"term": "mmrs-parity-twfpub"})),
    )
    .await;
    assert_eq!(status, 200, "{raw}");
    let teams: serde_json::Value = serde_json::from_str(&raw).expect("an array");
    assert_eq!(
        teams.as_array().map(Vec::len),
        Some(2),
        "both list permissions see the closed team too: {raw}"
    );

    common::delete_plain_user(&http, &admin, &plain.id).await;
}

/// `policy_id` is cleared before the search whatever the caller sent, so a body naming one
/// returns the same teams as a body that does not.
#[tokio::test]
async fn a_policy_id_in_the_body_is_ignored() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    create_team(&http, &token, "twfpol").await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::POST,
            base,
            &token,
            "/api/v4/teams/search",
            Some(&serde_json::json!({
                "term": "mmrs-parity-twfpol",
                "policy_id": "skjy5tackbqes3cwbzdoawkhtc",
            })),
        )
        .await;
        assert_eq!(status, 200, "{base}: {raw}");
        let teams: serde_json::Value = serde_json::from_str(&raw).expect("an array");
        assert_eq!(
            teams.as_array().map(Vec::len),
            Some(1),
            "{base}: the id is discarded, not applied as a filter: {raw}"
        );
        bodies.push(raw);
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// A malformed body is `SetInvalidParamWithErr("team_search")`; a `null` body is **not** an error
/// — Go's decoder leaves the struct zero-valued and the search runs with an empty term.
#[tokio::test]
async fn a_malformed_search_body_is_a_400_and_null_is_not() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    for body in ["", "not json", "[1,2]", r#"{"term":1}"#] {
        let (status, raw) = both_raw(
            &http,
            reqwest::Method::POST,
            &token,
            "/api/v4/teams/search",
            body,
        )
        .await;
        assert_eq!(status, 400, "body {body:?}: {raw}");
        assert_eq!(
            error_id(&raw),
            "api.context.invalid_body_param.app_error",
            "body {body:?}"
        );
    }

    let (status, _raw) = both_raw(
        &http,
        reqwest::Method::POST,
        &token,
        "/api/v4/teams/search",
        "null",
    )
    .await;
    assert_eq!(status, 200, "a null body is the zero-valued search");
}

// ---------------------------------------------------------------------------------------------
// createTeam
// ---------------------------------------------------------------------------------------------

/// Create one team on each server from the same body and compare everything that can match.
///
/// What is asserted beyond the reply: the **two default channels** exist with Go's translated
/// display names, the creator is a member, and the creator is a **team admin** — which happens
/// only because `CreateTeamWithUser` overwrites `team.Email` with the creator's address and
/// `JoinUserToTeam` then reads `team.Email == user.Email`. Drop that assignment and the creator
/// is an ordinary member of the team they just made.
#[tokio::test]
async fn creating_a_team_matches_go_down_to_the_default_channels() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    common::purge_api_fixtures().await;

    let mut created = Vec::new();
    for base in [GO, RUST] {
        let tag = if base == GO { "twfcrg" } else { "twfcrr" };
        let (status, raw) = send(
            &http,
            reqwest::Method::POST,
            base,
            &token,
            "/api/v4/teams",
            Some(&serde_json::json!({
                "name": format!("mmrs-parity-{tag}"),
                "display_name": format!("mmrs parity {tag}"),
                "type": "O",
                "description": "mmrs parity create",
                "company_name": "mmrs co",
                // **Discarded**: the app layer overwrites it with the creator's address.
                "email": "MMRS-Not-Applied@example.invalid",
                // **Discarded**: `TeamService.CreateTeam` clears it and `PreSave` mints a new one.
                "invite_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
                "delete_at": 12345,
            })),
        )
        .await;
        assert_eq!(status, 201, "{base}: {raw}");
        assert!(raw.ends_with('\n'), "{base}: the encoder leaves a newline");
        let team: serde_json::Value = serde_json::from_str(&raw).expect("a team");
        created.push(team);
    }

    let (go_team, rust_team) = (&created[0], &created[1]);
    for key in [
        "type",
        "description",
        "company_name",
        "delete_at",
        "allow_open_invite",
    ] {
        assert_eq!(go_team[key], rust_team[key], "{key}");
    }
    assert_eq!(
        // **`createTeam` keeps a submitted `delete_at`.** `Save` writes the column straight from
        // the struct and `PreSave` does not zero it, so a caller can create a team that is
        // already archived — where `updateTeam` discards the same field, because its app layer
        // copies seven named fields onto the stored row instead. Measured on both servers.
        go_team["delete_at"],
        12345,
        "the create path does not sanitise delete_at"
    );
    for team in [go_team, rust_team] {
        assert_ne!(
            team["invite_id"].as_str(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "the submitted invite id is discarded and a fresh one minted"
        );
        assert_eq!(
            team["invite_id"].as_str().map(str::len),
            Some(26),
            "and it is a real id"
        );
        assert_ne!(
            team["email"].as_str(),
            Some("mmrs-not-applied@example.invalid"),
            "the submitted email is overwritten with the creator's"
        );
        assert!(team["create_at"].as_i64().unwrap_or(0) > 0);
        assert_eq!(team["create_at"], team["update_at"], "PreSave sets both");
    }
    assert_eq!(
        go_team["email"], rust_team["email"],
        "both teams belong to the same creator"
    );

    // The default channels, and the creator's membership of each.
    let me: serde_json::Value = serde_json::from_str(
        &send(
            &http,
            reqwest::Method::GET,
            GO,
            &token,
            "/api/v4/users/me",
            None,
        )
        .await
        .1,
    )
    .expect("a user");
    let user_id = me["id"].as_str().expect("an id").to_owned();

    for (base, team) in [(GO, go_team), (RUST, rust_team)] {
        let team_id = team["id"].as_str().expect("an id");
        for (name, display_name) in [("town-square", "Town Square"), ("off-topic", "Off-Topic")] {
            let (status, raw) = send(
                &http,
                reqwest::Method::GET,
                GO,
                &token,
                &format!("/api/v4/teams/{team_id}/channels/name/{name}"),
                None,
            )
            .await;
            assert_eq!(status, 200, "{base}: {name} was not created: {raw}");
            let channel: serde_json::Value = serde_json::from_str(&raw).expect("a channel");
            assert_eq!(
                channel["display_name"].as_str(),
                Some(display_name),
                "{base}: {name}'s translated display name"
            );
            assert_eq!(channel["type"].as_str(), Some("O"), "{base}: {name}");
        }

        let (status, raw) = send(
            &http,
            reqwest::Method::GET,
            GO,
            &token,
            &format!("/api/v4/teams/{team_id}/members/{user_id}"),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: the creator is not a member: {raw}");
        let member: serde_json::Value = serde_json::from_str(&raw).expect("a member");
        assert_eq!(
            member["scheme_admin"], true,
            "{base}: the creator owns the team's contact address and joins as an admin"
        );
    }
}

/// A second team with the same **name** is a 400 whose id names a duplicate `id`, not a duplicate
/// name — the store reports the `teams_name_key` violation as an `id` field, and the app layer's
/// switch keys on that.
#[tokio::test]
async fn a_duplicate_team_name_is_the_existing_team_400() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let taken = create_team(&http, &token, "twfdup").await;
    assert!(!taken.is_empty());

    let mut answers = Vec::new();
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::POST,
            base,
            &token,
            "/api/v4/teams",
            Some(&serde_json::json!({
                "name": "mmrs-parity-twfdup",
                "display_name": "mmrs parity duplicate",
                "type": "O",
            })),
        )
        .await;
        assert_eq!(status, 400, "{base}: {raw}");
        answers.push(error_id(&raw));
    }
    assert_eq!(
        answers[0], "store.sql_team.save_team.existing.app_error",
        "a name collision is reported as an id collision"
    );
    assert_eq!(answers[0], answers[1]);
}

/// The refusals that do not need a second team: a body that will not decode, a body carrying an
/// `id`, and the model validations `IsValid` runs. Each is asked of both servers with the same
/// bytes and the status and error id compared.
#[tokio::test]
async fn the_create_refusals_agree() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    // A malformed body.
    for body in ["", "not json", "[1,2]", r#"{"name":1}"#] {
        let (status, raw) =
            both_raw(&http, reqwest::Method::POST, &token, "/api/v4/teams", body).await;
        assert_eq!(status, 400, "body {body:?}: {raw}");
        assert_eq!(error_id(&raw), "api.context.invalid_body_param.app_error");
    }

    // A client-supplied id: refused **before** `PreSave` would have assigned one.
    let (status, raw) = both_raw(
        &http,
        reqwest::Method::POST,
        &token,
        "/api/v4/teams",
        r#"{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"mmrs-parity-twfid","display_name":"mmrs parity id","type":"O"}"#,
    )
    .await;
    assert_eq!(status, 400, "{raw}");
    assert_eq!(
        error_id(&raw),
        "store.sql_team.save_team.existing.app_error"
    );

    // `IsValid`, three ways.
    for (body, expected) in [
        (
            r#"{"name":"mmrs-parity-twfnodn","display_name":"","type":"O"}"#,
            "model.team.is_valid.name.app_error",
        ),
        (
            r#"{"name":"","display_name":"mmrs parity noname","type":"O"}"#,
            // `characters`, not `url`: the empty name fails `IsValidTeamName`'s length check
            // before the URL-shape one, and the two ids are one branch apart.
            "model.team.is_valid.characters.app_error",
        ),
        (
            r#"{"name":"mmrs-parity-twfbadty","display_name":"mmrs parity bad type","type":"X"}"#,
            "model.team.is_valid.type.app_error",
        ),
    ] {
        let (status, raw) =
            both_raw(&http, reqwest::Method::POST, &token, "/api/v4/teams", body).await;
        assert_eq!(status, 400, "body {body}: {raw}");
        assert_eq!(error_id(&raw), expected, "body {body}");
    }
}
