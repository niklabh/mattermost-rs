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

use std::time::Duration;

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

// ---------------------------------------------------------------------------------------------
// removeTeamMember
// ---------------------------------------------------------------------------------------------

/// Everything one removal touches, asserted on both servers against the same user: the
/// membership row, the channel memberships, the `town-square` system post, the sidebar
/// categories, the team-category preferences and the user's `update_at`.
///
/// Two teams, one user. The user is a member of both, so the two removals differ only in which
/// server ran the cascade — which is what makes the *absence* of a step visible rather than the
/// normal state of a fresh fixture.
#[tokio::test]
async fn a_removal_cascades_the_same_way_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let go_team = create_team(&http, &admin, "twfrmg").await;
    let rust_team = create_team(&http, &admin, "twfrmr").await;
    let plain = common::create_plain_user(&http, &admin, &go_team, "twfremove").await;
    let plain_username = common::plain_username("twfremove");

    // The same user on the second team, and one extra private channel per team so the cascade has
    // something beyond `town-square`/`off-topic` to clear.
    add_user_to_team(&http, &admin, &rust_team, &plain.id).await;
    let mut extra = Vec::new();
    for (team, tag) in [(&go_team, "twfrmgc"), (&rust_team, "twfrmrc")] {
        let channel = common::create_channel_typed(&http, &admin, team, tag, "P").await;
        common::add_user_to_channel(&http, &admin, &channel, &plain.id).await;
        extra.push(channel);
    }

    // A preference in each team's category — the category *is* the team id — and a sidebar
    // category, which joining a team creates on its own.
    for team in [&go_team, &rust_team] {
        let (status, raw) = send(
            &http,
            reqwest::Method::PUT,
            GO,
            &plain.token,
            &format!("/api/v4/users/{}/preferences", plain.id),
            Some(&serde_json::json!([{
                "user_id": plain.id,
                "category": team,
                "name": "last_channel",
                "value": "mmrs",
            }])),
        )
        .await;
        assert_eq!(status, 200, "seeding the preference: {raw}");
    }

    let before: serde_json::Value = serde_json::from_str(
        &send(
            &http,
            reqwest::Method::GET,
            GO,
            &admin,
            &format!("/api/v4/users/{}", plain.id),
            None,
        )
        .await
        .1,
    )
    .expect("a user");
    let update_at_before = before["update_at"].as_i64().expect("an update_at");

    for (base, team_id, channel_id, expected_post) in [
        (
            GO,
            &go_team,
            &extra[0],
            (
                "system_remove_from_team",
                format!("{} removed from the team.", plain_username),
            ),
        ),
        (
            RUST,
            &rust_team,
            &extra[1],
            (
                "system_remove_from_team",
                format!("{} removed from the team.", plain_username),
            ),
        ),
    ] {
        // Sanity: the fixtures exist before the removal, or nothing below proves anything.
        plant_sidebar_channel(&plain.id, team_id, channel_id).await;
        let before_sidebar = sidebar_row_counts(&plain.id, team_id).await;
        if let Some((categories, channels)) = before_sidebar {
            assert!(
                categories > 0 && channels > 0,
                "{base}: the fixture must have sidebar rows to lose, got {categories}/{channels}"
            );
        }
        assert!(
            preference_exists(&http, &plain.token, &plain.id, team_id).await,
            "{base}: the seeded preference is there"
        );

        let (status, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &admin,
            &format!("/api/v4/teams/{team_id}/members/{}", plain.id),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {raw}");
        assert_eq!(
            raw, r#"{"status":"OK"}"#,
            "{base}: ReturnStatusOK, no newline"
        );

        // **The row is soft-deleted and stripped of its roles, and it is still readable.**
        // `getTeamMember` has no `DeleteAt` predicate — measured, not assumed: the first draft of
        // this test expected a 404 and Go answered 200. So the assertion has to be on the two
        // columns `RemoveTeamMember` writes, which is also the pair a port can get half right.
        let (status, raw) = send(
            &http,
            reqwest::Method::GET,
            GO,
            &admin,
            &format!("/api/v4/teams/{team_id}/members/{}", plain.id),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: the row survives the removal: {raw}");
        let member: serde_json::Value = serde_json::from_str(&raw).expect("a member");
        assert!(
            member["delete_at"].as_i64().unwrap_or(0) > 0,
            "{base}: DeleteAt is stamped: {raw}"
        );
        // **`Roles` is *not* cleared in the database**, on either server. `RemoveTeamMember`
        // assigns `teamMember.Roles = ""`, but `UpdateMember` writes `ExplicitRoles` into the
        // `Roles` column — so the assignment touches only the in-memory struct and the read still
        // computes `team_user` from `SchemeUser`. Measured on Go first; asserted here so a port
        // that "fixed" it by clearing the column would fail.
        assert_eq!(
            member["roles"].as_str(),
            Some("team_user"),
            "{base}: the scheme-derived roles survive"
        );

        // The private channel's membership is gone too.
        let (status, _raw) = send(
            &http,
            reqwest::Method::GET,
            GO,
            &admin,
            &format!("/api/v4/channels/{channel_id}/members/{}", plain.id),
            None,
        )
        .await;
        assert_eq!(status, 404, "{base}: the channel membership cascaded");

        // The sidebar and the team-category preferences are cleared.
        if before_sidebar.is_some() {
            assert_eq!(
                sidebar_row_counts(&plain.id, team_id).await,
                Some((0, 0)),
                "{base}: ClearSidebarOnTeamLeave empties both tables"
            );
        }
        assert!(
            !preference_exists(&http, &plain.token, &plain.id, team_id).await,
            "{base}: DeleteCategory(user, team_id)"
        );

        // The system post, authored by the removed user, in `town-square`.
        let town_square: serde_json::Value = serde_json::from_str(
            &send(
                &http,
                reqwest::Method::GET,
                GO,
                &admin,
                &format!("/api/v4/teams/{team_id}/channels/name/town-square"),
                None,
            )
            .await
            .1,
        )
        .expect("a channel");
        let town_square_id = town_square["id"].as_str().expect("an id");
        let posts: serde_json::Value = serde_json::from_str(
            &send(
                &http,
                reqwest::Method::GET,
                GO,
                &admin,
                &format!("/api/v4/channels/{town_square_id}/posts?per_page=50"),
                None,
            )
            .await
            .1,
        )
        .expect("a post list");
        let matched = posts["posts"]
            .as_object()
            .expect("a post map")
            .values()
            .find(|post| post["type"] == expected_post.0 && post["user_id"] == plain.id.as_str());
        let matched =
            matched.unwrap_or_else(|| panic!("{base}: no {} post in town-square", expected_post.0));
        assert_eq!(
            matched["message"].as_str(),
            Some(expected_post.1.as_str()),
            "{base}: the message text"
        );
        assert_eq!(
            matched["props"]["username"].as_str(),
            Some(plain_username.as_str()),
            "{base}: the username prop"
        );
    }

    // `UpdateUpdateAt` ran at least once.
    let after: serde_json::Value = serde_json::from_str(
        &send(
            &http,
            reqwest::Method::GET,
            GO,
            &admin,
            &format!("/api/v4/users/{}", plain.id),
            None,
        )
        .await
        .1,
    )
    .expect("a user");
    assert!(
        after["update_at"].as_i64().unwrap_or(0) > update_at_before,
        "postProcessTeamMemberLeave bumps Users.UpdateAt"
    );

    // **A second removal of the same user is a 200, not a 400.** `GetTeamMember` has no
    // `DeleteAt` predicate, so `LeaveTeam` finds the soft-deleted row and runs the whole cascade
    // again — the route is idempotent. Measured on Go first; the 400
    // (`api.team.remove_user_from_team.missing.app_error`) is reachable only for a user who was
    // *never* a member, which the next loop asks for.
    for (base, team_id) in [(GO, &go_team), (RUST, &rust_team)] {
        let (status, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &admin,
            &format!("/api/v4/teams/{team_id}/members/{}", plain.id),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: removing twice is idempotent: {raw}");
    }

    // A user who was never on the team is the 400.
    let stranger = common::create_plain_user(&http, &admin, &go_team, "twfstranger").await;
    for (base, team_id) in [(GO, &go_team), (RUST, &rust_team)] {
        // `twfstranger` joined `go_team`, so only `rust_team` can answer the never-a-member case;
        // the `go_team` pass is the *removal* that makes the next one meaningful.
        let (status, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &admin,
            &format!("/api/v4/teams/{team_id}/members/{}", stranger.id),
            None,
        )
        .await;
        if team_id == &rust_team {
            assert_eq!(status, 400, "{base}: never a member: {raw}");
            assert_eq!(
                error_id(&raw),
                "api.team.remove_user_from_team.missing.app_error",
                "{base}"
            );
        } else {
            assert_eq!(status, 200, "{base}: {raw}");
        }
    }
    common::delete_plain_user(&http, &admin, &stranger.id).await;

    common::delete_plain_user(&http, &admin, &plain.id).await;
}

/// **Leaving is not a permission.** The gate is inside `if session.UserId != params.UserId`, so an
/// ordinary member with no team permissions can remove themselves — and the same member removing
/// *someone else* is a 403. The leave also posts the other message: "left", not "removed".
#[tokio::test]
async fn a_self_removal_needs_no_permission_and_posts_the_other_message() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let team_id = create_team(&http, &admin, "twfself").await;
    let leaver = common::create_plain_user(&http, &admin, &team_id, "twfleave").await;
    let leaver_username = common::plain_username("twfleave");
    let bystander = common::create_plain_user(&http, &admin, &team_id, "twfstay").await;

    // Removing someone else is a 403 on both servers.
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &leaver.token,
            &format!("/api/v4/teams/{team_id}/members/{}", bystander.id),
            None,
        )
        .await;
        assert_eq!(status, 403, "{base}: {raw}");
    }

    // Removing yourself is a 200, from Rust, with no permission at all.
    let (status, raw) = send(
        &http,
        reqwest::Method::DELETE,
        RUST,
        &leaver.token,
        &format!("/api/v4/teams/{team_id}/members/{}", leaver.id),
        None,
    )
    .await;
    assert_eq!(status, 200, "a self-removal needs no permission: {raw}");

    let town_square: serde_json::Value = serde_json::from_str(
        &send(
            &http,
            reqwest::Method::GET,
            GO,
            &admin,
            &format!("/api/v4/teams/{team_id}/channels/name/town-square"),
            None,
        )
        .await
        .1,
    )
    .expect("a channel");
    let town_square_id = town_square["id"].as_str().expect("an id");
    let posts: serde_json::Value = serde_json::from_str(
        &send(
            &http,
            reqwest::Method::GET,
            GO,
            &admin,
            &format!("/api/v4/channels/{town_square_id}/posts?per_page=50"),
            None,
        )
        .await
        .1,
    )
    .expect("a post list");
    let matched = posts["posts"]
        .as_object()
        .expect("a post map")
        .values()
        .find(|post| post["user_id"] == leaver.id.as_str() && post["type"] == "system_leave_team")
        .expect("a system_leave_team post");
    assert_eq!(
        matched["message"].as_str(),
        Some(format!("{} left the team.", leaver_username).as_str()),
        "a self-removal posts `left the team`, not `removed from the team`"
    );

    common::delete_plain_user(&http, &admin, &leaver.id).await;
    common::delete_plain_user(&http, &admin, &bystander.id).await;
}

/// Add `user_id` to `team_id` through Go's API.
async fn add_user_to_team(http: &reqwest::Client, admin_token: &str, team_id: &str, user_id: &str) {
    let (status, raw) = send(
        http,
        reqwest::Method::POST,
        GO,
        admin_token,
        &format!("/api/v4/teams/{team_id}/members"),
        Some(&serde_json::json!({ "team_id": team_id, "user_id": user_id })),
    )
    .await;
    assert!(
        (200..300).contains(&status),
        "adding {user_id} to {team_id} failed: {raw}"
    );
}

/// Put `channel_id` into one of the user's sidebar categories for `team_id`, by hand.
///
/// Joining a team creates the three default **categories** and no `SidebarChannels` rows at all
/// — measured: 3/0. So without this the first of `ClearSidebarOnTeamLeave`'s two statements has
/// nothing to delete and any mutation of it survives. Returns whether the row was planted.
async fn plant_sidebar_channel(user_id: &str, team_id: &str, channel_id: &str) -> bool {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return false;
    };
    let category: Option<String> = sqlx::query_scalar(
        "SELECT id FROM sidebarcategories WHERE userid = $1 AND teamid = $2 ORDER BY id LIMIT 1",
    )
    .bind(user_id)
    .bind(team_id)
    .fetch_optional(&pool)
    .await
    .expect("the category lookup runs");
    let Some(category) = category else {
        return false;
    };
    sqlx::query(
        "INSERT INTO sidebarchannels (channelid, userid, categoryid, sortorder) \
         VALUES ($1, $2, $3, 0) ON CONFLICT DO NOTHING",
    )
    .bind(channel_id)
    .bind(user_id)
    .bind(&category)
    .execute(&pool)
    .await
    .expect("the sidebar channel is planted");
    true
}

/// The user's sidebar rows for one team, **read straight from the database**.
///
/// Not through `GET /users/{id}/teams/{id}/channels/categories`: that handler *creates* the
/// initial categories when it finds none, so it answers non-empty however well the cascade
/// worked. The first draft of this test used it and failed against Go.
async fn sidebar_row_counts(user_id: &str, team_id: &str) -> Option<(i64, i64)> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()?;
    let categories: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sidebarcategories WHERE userid = $1 AND teamid = $2",
    )
    .bind(user_id)
    .bind(team_id)
    .fetch_one(&pool)
    .await
    .expect("the category count runs");
    let channels: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sidebarchannels sc \
          JOIN sidebarcategories cat ON sc.categoryid = cat.id \
         WHERE sc.userid = $1 AND cat.teamid = $2",
    )
    .bind(user_id)
    .bind(team_id)
    .fetch_one(&pool)
    .await
    .expect("the channel count runs");
    Some((categories, channels))
}

async fn preference_exists(
    http: &reqwest::Client,
    token: &str,
    user_id: &str,
    category: &str,
) -> bool {
    let (status, raw) = send(
        http,
        reqwest::Method::GET,
        GO,
        token,
        &format!("/api/v4/users/{user_id}/preferences/{category}"),
        None,
    )
    .await;
    status == 200
        && serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.as_array().map(|rows| !rows.is_empty()))
            .unwrap_or(false)
}

// ---------------------------------------------------------------------------------------------
// invalidateAllEmailInvites
// ---------------------------------------------------------------------------------------------

/// The route voids **every** outstanding invitation on the installation, so both halves of the
/// comparison plant their own token row and check it is gone.
///
/// Tokens are planted directly: minting a real one needs `POST /teams/{id}/invite/email`, which
/// needs a working SMTP server. The row is all this route looks at.
#[tokio::test]
async fn invalidating_email_invites_removes_both_token_types() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;

    for base in [GO, RUST] {
        let tag = if base == GO { "twfinvg" } else { "twfinvr" };
        let planted = plant_invite_tokens(tag).await;
        if !planted {
            return;
        }
        assert_eq!(
            invite_token_count(tag).await,
            Some(2),
            "{base}: the fixture tokens are there"
        );

        let (status, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &admin,
            "/api/v4/teams/invites/email",
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {raw}");
        assert_eq!(
            raw, r#"{"status":"OK"}"#,
            "{base}: ReturnStatusOK, no newline"
        );

        assert_eq!(
            invite_token_count(tag).await,
            Some(0),
            "{base}: both token types are gone, live ones included"
        );
    }
}

/// An ordinary member does not hold `invalidate_email_invite`, so the route is a 403 with the
/// same id on both servers.
#[tokio::test]
async fn invalidating_email_invites_needs_the_permission() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let team_id = create_team(&http, &admin, "twfinvp").await;
    let plain = common::create_plain_user(&http, &admin, &team_id, "twfinvperm").await;

    let mut answers = Vec::new();
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &plain.token,
            "/api/v4/teams/invites/email",
            None,
        )
        .await;
        assert_eq!(status, 403, "{base}: {raw}");
        answers.push(error_id(&raw));
    }
    assert_eq!(answers[0], answers[1], "the same refusal id");

    common::delete_plain_user(&http, &admin, &plain.id).await;
}

/// One `team_invitation` and one `guest_invitation` row, tagged so the count below can find
/// exactly these two. Returns false when there is no database to reach.
async fn plant_invite_tokens(tag: &str) -> bool {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return false;
    };
    for (suffix, token_type) in [("t", "team_invitation"), ("g", "guest_invitation")] {
        let token = format!("mmrs{tag}{suffix}000000000000000000000000")
            .chars()
            .take(26)
            .collect::<String>();
        sqlx::query(
            "INSERT INTO tokens (token, createat, type, extra) VALUES ($1, $2, $3, 'mmrs') \
             ON CONFLICT (token) DO NOTHING",
        )
        .bind(&token)
        .bind(1_700_000_000_000_i64)
        .bind(token_type)
        .execute(&pool)
        .await
        .expect("the invite token is planted");
    }
    true
}

async fn invite_token_count(tag: &str) -> Option<i64> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tokens WHERE token LIKE $1")
        .bind(format!("mmrs{tag}%"))
        .fetch_one(&pool)
        .await
        .expect("the token count runs");
    Some(count)
}

// ---------------------------------------------------------------------------------------------
// deleteTeam
// ---------------------------------------------------------------------------------------------

/// Archiving stamps `delete_at` and touches nothing else, and `restoreTeam` is its exact inverse.
///
/// Each server archives a team of its own, and both teams are restored before the test ends —
/// the fixture teams are shared state and a suite that leaves one archived breaks the next.
#[tokio::test]
async fn archiving_a_team_stamps_delete_at_and_nothing_else() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    let mut shapes = Vec::new();
    for base in [GO, RUST] {
        let tag = if base == GO { "twfdelg" } else { "twfdelr" };
        let team_id = create_team(&http, &token, tag).await;

        let before: serde_json::Value = serde_json::from_str(
            &send(
                &http,
                reqwest::Method::GET,
                base,
                &token,
                &format!("/api/v4/teams/{team_id}"),
                None,
            )
            .await
            .1,
        )
        .expect("a team");

        let (status, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &token,
            &format!("/api/v4/teams/{team_id}"),
            None,
        )
        .await;
        assert_eq!(status, 200, "{base}: {raw}");
        assert_eq!(raw, r#"{"status":"OK"}"#, "{base}: ReturnStatusOK");

        let after: serde_json::Value = serde_json::from_str(
            &send(
                &http,
                reqwest::Method::GET,
                base,
                &token,
                &format!("/api/v4/teams/{team_id}"),
                None,
            )
            .await
            .1,
        )
        .expect("a team");
        assert!(
            after["delete_at"].as_i64().unwrap_or(0) > 0,
            "{base}: delete_at is stamped: {after}"
        );

        // Everything else survives, `update_at` excepted — `PreUpdate` moves it.
        for key in [
            "id",
            "name",
            "display_name",
            "description",
            "email",
            "type",
            "company_name",
            "allowed_domains",
            "invite_id",
            "allow_open_invite",
            "create_at",
        ] {
            assert_eq!(before[key], after[key], "{base}: {key} must not move");
        }
        assert!(
            after["update_at"].as_i64().unwrap_or(0) >= before["update_at"].as_i64().unwrap_or(0),
            "{base}: PreUpdate moves update_at"
        );

        // Restore, so the archived team does not outlive this test.
        let (status, raw) = send(
            &http,
            reqwest::Method::POST,
            base,
            &token,
            &format!("/api/v4/teams/{team_id}/restore"),
            Some(&serde_json::json!({})),
        )
        .await;
        assert_eq!(status, 200, "{base}: restoring the team: {raw}");
        let restored: serde_json::Value = serde_json::from_str(&raw).expect("a team");
        assert_eq!(restored["delete_at"], 0, "{base}: restore is the inverse");

        // The *shape* of the answer, with the per-team values normalised away.
        let mut shape = after.clone();
        if let Some(object) = shape.as_object_mut() {
            for key in ["id", "name", "display_name", "email", "invite_id"] {
                object.insert(key.to_owned(), serde_json::json!(true));
            }
            for key in ["create_at", "update_at", "delete_at"] {
                let nonzero = object
                    .get(key)
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0)
                    > 0;
                object.insert(key.to_owned(), serde_json::json!(nonzero));
            }
        }
        shapes.push(shape);
    }
    assert_eq!(
        shapes[0], shapes[1],
        "the archived teams have the same shape"
    );
}

/// `?permanent=true` with `EnableAPITeamDeletion` off — the stock configuration — is a **401**,
/// and the id depends on whether the caller is a system admin. Both halves are asserted, because
/// the two ids differ by ten characters at the same status.
///
/// `strconv.ParseBool` discards its error, so `?permanent=yes` is *false* and archives the team.
#[tokio::test]
async fn permanent_deletion_is_a_401_whose_id_depends_on_the_caller() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let team_id = create_team(&http, &admin, "twfperm2").await;

    // The admin gets the verbose id.
    let mut answers = Vec::new();
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &admin,
            &format!("/api/v4/teams/{team_id}?permanent=true"),
            None,
        )
        .await;
        assert_eq!(status, 401, "{base}: not 403: {raw}");
        answers.push(error_id(&raw));
    }
    assert_eq!(
        answers[0], "api.user.delete_team.not_enabled.for_admin.app_error",
        "a system admin gets the verbose id"
    );
    assert_eq!(answers[0], answers[1]);

    // A non-admin who nonetheless holds `manage_team` gets the short id. `team_admin` grants
    // `manage_team`, and the plain user is made one through Go.
    let plain = common::create_plain_user(&http, &admin, &team_id, "twfpermdel").await;
    let (status, raw) = send(
        &http,
        reqwest::Method::PUT,
        GO,
        &admin,
        &format!("/api/v4/teams/{team_id}/members/{}/roles", plain.id),
        Some(&serde_json::json!({"roles": "team_user team_admin"})),
    )
    .await;
    assert_eq!(status, 200, "promoting the fixture user: {raw}");

    let mut answers = Vec::new();
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::DELETE,
            base,
            &plain.token,
            &format!("/api/v4/teams/{team_id}?permanent=true"),
            None,
        )
        .await;
        assert_eq!(status, 401, "{base}: {raw}");
        answers.push(error_id(&raw));
    }
    assert_eq!(
        answers[0], "api.user.delete_team.not_enabled.app_error",
        "a non-admin gets the short id"
    );
    assert_eq!(answers[0], answers[1]);

    // `?permanent=yes` is not a bool, so it archives instead of refusing.
    let (status, raw) = send(
        &http,
        reqwest::Method::DELETE,
        RUST,
        &admin,
        &format!("/api/v4/teams/{team_id}?permanent=yes"),
        None,
    )
    .await;
    assert_eq!(status, 200, "ParseBool discards its error: {raw}");
    let (status, _raw) = send(
        &http,
        reqwest::Method::POST,
        RUST,
        &admin,
        &format!("/api/v4/teams/{team_id}/restore"),
        Some(&serde_json::json!({})),
    )
    .await;
    assert_eq!(status, 200, "and the team is restored");

    common::delete_plain_user(&http, &admin, &plain.id).await;
}

/// The permission gate precedes both the archive and the permanent refusal, so a caller without
/// `manage_team` gets a 403 even for `?permanent=true`.
#[tokio::test]
async fn archiving_needs_manage_team_before_anything_else() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let team_id = create_team(&http, &admin, "twfdelp").await;
    let plain = common::create_plain_user(&http, &admin, &team_id, "twfdelperm").await;

    for suffix in ["", "?permanent=true"] {
        for base in [GO, RUST] {
            let (status, raw) = send(
                &http,
                reqwest::Method::DELETE,
                base,
                &plain.token,
                &format!("/api/v4/teams/{team_id}{suffix}"),
                None,
            )
            .await;
            assert_eq!(status, 403, "{base} for {suffix:?}: {raw}");
        }
    }

    common::delete_plain_user(&http, &admin, &plain.id).await;
}
