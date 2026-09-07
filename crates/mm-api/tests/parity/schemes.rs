//! Cross-server parity for all seven `/api/v4/schemes` routes.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity schemes
//! ```
//!
//! # The fixture has to be planted, and that is the point
//!
//! `POST /api/v4/schemes` answers **501** on an unlicensed server, so nothing this suite can do
//! through the API creates a scheme — and without one, all four reads return `[]` on both servers
//! and every assertion about their contents passes vacuously. `common::plant_scheme` writes the
//! rows directly; `purge_api_fixtures` removes them, detaching the teams and channels first.
//!
//! # Three writers across seven routes
//!
//! `getSchemes` and `getTeamsForScheme` use `json.Marshal` (no newline); `getScheme` and
//! `getChannelsForScheme` use `json.NewEncoder(w).Encode` (newline). The pairs are *crossed*:
//! the list of schemes and the list of teams agree with each other and disagree with the single
//! scheme and the list of channels. Asserted per route, because reading the Go file top to bottom
//! suggests the opposite grouping.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, RUST, client, fetch_both, fetch_both_raw, go_minted_token, stack_enabled,
};

/// The shared fixture: three team-scoped schemes, one channel-scoped, three teams on the first
/// team scheme, two channels on the channel scheme, and three single-permission roles.
///
/// # Every part of it exists because a mutation survived without it
///
/// | Piece | What it makes visible |
/// |---|---|
/// | **three** team schemes | `page * per_page` — at page 0, or with fewer rows than a page, the offset is the page and a mutation of the arithmetic is invisible |
/// | **three** teams, display names reversed against their names | `ORDER BY DisplayName` against `ORDER BY Name` |
/// | a **board** channel (`BO`) | `Type <> 'S'` against `Type IN ('O','P','D','G')` — the two constants sit thirteen lines apart in the Go source |
/// | three single-permission roles | the three sysconsole reads, which **no stock role separates**, and `SanitizeTeams`, which is a no-op for an admin |
static FIXTURE: tokio::sync::OnceCell<Option<Fixture>> = tokio::sync::OnceCell::const_new();

#[derive(Debug, Clone)]
struct Fixture {
    team_scheme: String,
    channel_scheme: String,
    /// Attached to `team_scheme`, in display-name order — which is the **reverse** of their name
    /// order.
    teams_by_display_name: Vec<String>,
    channel_id: String,
    board_id: String,
    /// A session holding `sysconsole_read_user_management_permissions` and nothing else.
    permissions_reader: String,
    /// `sysconsole_read_user_management_teams` and nothing else — so it may list a scheme's teams
    /// and may **not** manage them, which is what makes the sanitizer observable.
    teams_reader: String,
    channels_reader: String,
}

async fn fixture(client: &reqwest::Client, token: &str) -> Option<Fixture> {
    FIXTURE
        .get_or_init(|| async {
            // **The purge first, and explicitly.** `purge_api_fixtures` is a `OnceCell` that
            // `create_team` triggers, and it deletes every `mmrsscheme%` row — so planting before
            // the first `create_team` in this binary has the purge delete the fixture moments
            // after it is written. That is what happened: the list came back `[]` and every
            // assertion about its contents failed.
            common::purge_api_fixtures().await;

            let team_scheme = common::plant_scheme("team", "teamscope").await?;
            let other_team_scheme = common::plant_scheme("team", "teamtwo").await?;
            common::plant_scheme("team", "teamthree").await?;
            let channel_scheme = common::plant_scheme("channel", "chanscope").await?;
            let other_channel_scheme = common::plant_scheme("channel", "chantwo").await?;

            // Three teams on the first team scheme. `create_team` derives the display name from
            // the same tag as the name, so they sort alike until renamed — and then the two
            // orderings disagree, which is the point.
            let mut teams = Vec::new();
            for (tag, display) in [
                ("schemefixa", "zzz mmrs first-by-name"),
                ("schemefixb", "mmm mmrs second-by-name"),
                ("schemefixc", "aaa mmrs third-by-name"),
            ] {
                let id = common::create_team(client, token, tag).await;
                common::set_team_display_name(client, token, &id, display).await;
                common::set_team_scheme(&id, Some(&team_scheme)).await;
                teams.push(id);
            }
            // Display-name order is c, b, a — the reverse of the name order the tags give.
            let teams_by_display_name = vec![teams[2].clone(), teams[1].clone(), teams[0].clone()];

            let channel_id = common::create_channel(client, token, &teams[0], "schemefix").await;
            common::set_channel_scheme(&channel_id, Some(&channel_scheme)).await;
            let board_id = common::plant_channel_of_type(&teams[0], "BO", "schemeboard").await?;
            common::set_channel_scheme(&board_id, Some(&channel_scheme)).await;

            // **A second scheme with its own team and channel.** Without these, every team that
            // has *any* scheme has *this* scheme, so `WHERE SchemeId = $1` and
            // `WHERE SchemeId IS NOT NULL` return the same rows and a mutation dropping the
            // predicate survives. Measured, twice.
            let other_team = common::create_team(client, token, "schemeother").await;
            common::set_team_scheme(&other_team, Some(&other_team_scheme)).await;
            let other_channel =
                common::create_channel(client, token, &other_team, "schemeother").await;
            common::set_channel_scheme(&other_channel, Some(&other_channel_scheme)).await;

            let reader = async |tag: &str, permission: &str| -> String {
                let role = common::plant_role(tag, permission).await.expect("planted");
                let user = common::create_plain_user(client, token, &teams[0], tag).await;
                common::set_user_roles(&user.id, &format!("system_user {role}")).await;
                // A fresh token: `session.Roles` is copied at login and never re-read.
                common::login_plain_user(client, tag).await
            };

            Some(Fixture {
                permissions_reader: reader(
                    "schemeperms",
                    "sysconsole_read_user_management_permissions",
                )
                .await,
                teams_reader: reader("schemeteams", "sysconsole_read_user_management_teams").await,
                channels_reader: reader("schemechans", "sysconsole_read_user_management_channels")
                    .await,
                team_scheme,
                channel_scheme,
                teams_by_display_name,
                channel_id,
                board_id,
            })
        })
        .await
        .clone()
}

/// `GET /api/v4/schemes`, unfiltered and for each accepted scope.
#[tokio::test]
async fn the_scheme_list_matches_for_every_accepted_scope() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let Some(fixture) = fixture(&client, &token).await else {
        return; // no DATABASE_URL
    };

    for query in ["", "?scope=team", "?scope=channel", "?scope=&per_page=200"] {
        let path = format!("/api/v4/schemes{query}");
        let (go, rs) = fetch_both(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
        assert!(
            !rs.ends_with(b"\n"),
            "`getSchemes` is `json.Marshal` and a bare `Write`: {path}"
        );
    }

    // The fixture is actually visible, so the comparisons above are not two empty arrays.
    let (all, _) = fetch_both(&client, &token, "/api/v4/schemes?per_page=200").await;
    let all: Vec<serde_json::Value> = serde_json::from_slice(&all).expect("an array");
    let ids: Vec<&str> = all.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&fixture.team_scheme.as_str()),
        "the planted team scheme is missing from the unfiltered list: {ids:?}"
    );
    assert!(ids.contains(&fixture.channel_scheme.as_str()));

    // **`page * per_page` on the scheme list.** Three team-scoped schemes and two per page, so
    // page 1 holds one; an offset of `page` would hold two.
    //
    // **Both bodies are compared.** The first version of this discarded ours — `let (page_one, _)`
    // — and asserted only on Go's length, so the offset mutation survived: the assertion was
    // about the Go server, which is never mutated. Measured, twice: the fix was written once
    // against a line `cargo fmt` had already split, so it silently did not apply.
    let path = "/api/v4/schemes?scope=team&per_page=2&page=1";
    let (go_page_one, rs_page_one) = fetch_both(&client, &token, path).await;
    assert_eq!(
        String::from_utf8_lossy(&go_page_one),
        String::from_utf8_lossy(&rs_page_one),
        "{path}"
    );
    let page_one: Vec<serde_json::Value> = serde_json::from_slice(&go_page_one).expect("an array");
    assert_eq!(
        page_one.len(),
        1,
        "an offset of `page` rather than `page * per_page` would return two: {page_one:?}"
    );

    // And the scope filter really filters.
    let (team_only, _) =
        fetch_both(&client, &token, "/api/v4/schemes?scope=team&per_page=200").await;
    let team_only: Vec<serde_json::Value> = serde_json::from_slice(&team_only).expect("an array");
    assert!(
        team_only
            .iter()
            .all(|s| s["scope"].as_str() == Some("team")),
        "?scope=team must return only team schemes: {team_only:?}"
    );
    assert!(
        team_only
            .iter()
            .any(|s| s["id"].as_str() == Some(fixture.team_scheme.as_str())),
        "and it must still contain ours"
    );
}

/// An unaccepted `?scope=` is a 400 — including two values that are real `SchemeScope`s.
#[tokio::test]
async fn an_unaccepted_scope_is_refused() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // Two of these are real `SchemeScope` values that this handler still refuses, one differs
    // only in case, and one is a bare space — checked against the running Go server rather than
    // assumed, because "the handler accepts every scope the model defines" is the intuitive
    // reading and it is wrong.
    for scope in ["playbook", "run", "Team", "nonsense", "%20"] {
        let path = format!("/api/v4/schemes?scope={scope}");
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
        let body = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(body["id"], "api.context.invalid_body_param.app_error");
    }
}

/// `GET /api/v4/schemes/{scheme_id}`, and the 404 for one that does not exist.
#[tokio::test]
async fn a_single_scheme_matches_and_a_missing_one_is_a_404() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let Some(fixture) = fixture(&client, &token).await else {
        return;
    };

    let path = format!("/api/v4/schemes/{}", fixture.team_scheme);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    assert!(
        rs.ends_with(b"\n"),
        "`getScheme` is `json.NewEncoder(w).Encode` — unlike `getSchemes` twenty lines above it"
    );

    let scheme: serde_json::Value = serde_json::from_slice(&go).expect("an object");
    assert_eq!(scheme["scope"], "team");
    assert_eq!(
        scheme.as_object().expect("an object").len(),
        18,
        "every `model.Scheme` field is on the wire — eighteen, including the four playbook and \
         run role names that no unlicensed server ever fills: {scheme}"
    );

    // A well-formed id that is no scheme.
    let missing = "/api/v4/schemes/zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, missing).await;
    assert_eq!(go_status, 404, "{missing}");
    assert_eq!(rs_status, go_status, "{missing}");
    let body = common::assert_error_bodies_match_except_known_gaps(&go, &rs, missing);
    assert_eq!(body["id"], "app.scheme.get.app_error");

    // And an id that is not an id at all — a 400 from `RequireSchemeId`, before the store.
    let bad = "/api/v4/schemes/short";
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, bad).await;
    assert_eq!(go_status, 400, "{bad}");
    assert_eq!(rs_status, go_status, "{bad}");
    let body = common::assert_error_bodies_match_except_known_gaps(&go, &rs, bad);
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");
}

/// The two scope-scoped listings, the **400 each gives for the other's scope**, the ordering, the
/// paging arithmetic, the board channel and the sanitizer.
#[tokio::test]
async fn the_scoped_listings_match_and_refuse_the_wrong_scope() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let Some(fixture) = fixture(&client, &token).await else {
        return;
    };

    let teams_path = format!("/api/v4/schemes/{}/teams?per_page=200", fixture.team_scheme);
    let (go, rs) = fetch_both(&client, &token, &teams_path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{teams_path}"
    );
    assert!(
        !rs.ends_with(b"\n"),
        "`getTeamsForScheme` is `json.Marshal` and a bare `Write`"
    );

    // **`ORDER BY DisplayName`, not `Name`.** The fixture's display names are the reverse of its
    // names, so the two orderings are distinguishable — which they are not for any team this
    // suite creates from a single tag.
    let teams: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("an array");
    let ids: Vec<&str> = teams.iter().filter_map(|t| t["id"].as_str()).collect();
    let expected: Vec<&str> = fixture
        .teams_by_display_name
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(
        ids, expected,
        "display-name order, which reverses the names"
    );

    // **`page * per_page`.** At page 0 the offset is the page whatever the arithmetic, so the
    // second page is the only place a mutation of it shows.
    let page_one = format!(
        "/api/v4/schemes/{}/teams?per_page=2&page=1",
        fixture.team_scheme
    );
    let (go, rs) = fetch_both(&client, &token, &page_one).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{page_one}"
    );
    let page_one_teams: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("an array");
    assert_eq!(
        page_one_teams.len(),
        1,
        "three teams, two per page: page 1 holds the third alone — an offset of `page` rather \
         than `page * per_page` would hold two: {page_one_teams:?}"
    );
    assert_eq!(page_one_teams[0]["id"].as_str(), Some(expected[2]));

    let channels_path = format!(
        "/api/v4/schemes/{}/channels?per_page=200",
        fixture.channel_scheme
    );
    let (go, rs) = fetch_both(&client, &token, &channels_path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{channels_path}"
    );
    assert!(
        rs.ends_with(b"\n"),
        "`getChannelsForScheme` is `json.NewEncoder(w).Encode` — the opposite of its twin"
    );
    let channels: Vec<serde_json::Value> = serde_json::from_slice(&go).expect("an array");
    let channel_ids: Vec<&str> = channels.iter().filter_map(|c| c["id"].as_str()).collect();
    assert!(channel_ids.contains(&fixture.channel_id.as_str()));
    // **The board.** `GetChannelsByScheme` excludes only `'S'`; `ChannelStore::Get` excludes
    // everything but `('O','P','D','G')`. Without a board in the fixture the two agree.
    assert!(
        channel_ids.contains(&fixture.board_id.as_str()),
        "a board channel is in a scheme's channel list — only spaces are excluded: {channel_ids:?}"
    );
    assert_eq!(
        channel_ids.len(),
        2,
        "and **only** this scheme's channels: another scheme's channel exists and must not \
         appear, which is what tells `SchemeId = $1` from `SchemeId IS NOT NULL`: {channel_ids:?}"
    );

    // **Each refuses the other's scope with a 400, not a 404 and not an empty list.**
    let wrong_teams = format!("/api/v4/schemes/{}/teams", fixture.channel_scheme);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &wrong_teams).await;
    assert_eq!(go_status, 400, "{wrong_teams}");
    assert_eq!(rs_status, go_status, "{wrong_teams}");
    let body = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &wrong_teams);
    assert_eq!(body["id"], "api.scheme.get_teams_for_scheme.scope.error");

    let wrong_channels = format!("/api/v4/schemes/{}/channels", fixture.team_scheme);
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &wrong_channels).await;
    assert_eq!(go_status, 400, "{wrong_channels}");
    assert_eq!(rs_status, go_status, "{wrong_channels}");
    let body = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &wrong_channels);
    assert_eq!(body["id"], "api.scheme.get_channels_for_scheme.scope.error");
}

/// **The three sysconsole reads are three different permissions**, and `SanitizeTeams` does
/// something.
///
/// No stock role separates the three, so this uses planted single-permission roles. Each reader
/// gets its own route and is refused the other two — which is the only way a mutation swapping one
/// permission for another is visible at all.
#[tokio::test]
async fn each_read_needs_its_own_sysconsole_permission() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let Some(fixture) = fixture(&client, &token).await else {
        return;
    };

    let list = "/api/v4/schemes".to_owned();
    let teams = format!("/api/v4/schemes/{}/teams", fixture.team_scheme);
    let channels = format!("/api/v4/schemes/{}/channels", fixture.channel_scheme);

    for (reader, allowed) in [
        (&fixture.permissions_reader, &list),
        (&fixture.teams_reader, &teams),
        (&fixture.channels_reader, &channels),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, reader, allowed).await;
        assert_eq!(
            go_status,
            200,
            "the matching permission admits: {allowed} -> {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{allowed}");
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{allowed}"
        );

        for refused in [&list, &teams, &channels] {
            if refused == allowed {
                continue;
            }
            let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, reader, refused).await;
            assert_eq!(
                go_status, 403,
                "a different permission is refused: {refused}"
            );
            assert_eq!(rs_status, go_status, "{refused}");
            common::assert_error_bodies_match_except_known_gaps(&go, &rs, refused);
        }
    }

    // **The sanitizer.** The teams reader holds no `manage_team` on any of these teams, so
    // `SanitizeTeams` blanks `email` and `allowed_domains` — where the admin above sees them.
    // An admin-only fixture cannot observe this at all: an admin can manage every team.
    let ((_, reader_body), _) = fetch_both_raw(&client, &fixture.teams_reader, &teams).await;
    let reader_teams: Vec<serde_json::Value> = serde_json::from_slice(&reader_body).expect("json");
    assert!(!reader_teams.is_empty());
    for team in &reader_teams {
        assert_eq!(
            team["email"], "",
            "a reader who cannot manage the team gets no email: {team}"
        );
    }

    let ((_, admin_body), _) = fetch_both_raw(&client, &token, &teams).await;
    let admin_teams: Vec<serde_json::Value> = serde_json::from_slice(&admin_body).expect("json");
    assert!(
        admin_teams.iter().any(|t| t["email"] != ""),
        "and the admin does see one, so the assertion above is about sanitization and not about \
         the column being empty: {admin_teams:?}"
    );
}

/// The three writes, all 501 on an unlicensed server — and the **400 that comes first**.
#[tokio::test]
async fn the_writes_are_all_licence_refusals_in_gos_own_order() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let Some(fixture) = fixture(&client, &token).await else {
        return;
    };

    let send = async |method: reqwest::Method, path: &str, body: &'static [u8]| {
        let call = async |base: &str| {
            let response = client
                .request(method.clone(), format!("{base}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .body(body.to_vec())
                .send()
                .await
                .expect("reachable");
            let status = response.status().as_u16();
            let served_by = response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            (
                status,
                served_by,
                response.bytes().await.expect("reads").to_vec(),
            )
        };
        let (go_status, _, go) = call(common::GO).await;
        let (rs_status, served_by, rs) = call(RUST).await;
        assert_eq!(rs_status, go_status, "{path}");
        assert_eq!(
            served_by.as_deref(),
            Some("rust"),
            "{path} is ours to answer"
        );
        common::assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        (go_status, go)
    };

    let scheme = format!("/api/v4/schemes/{}", fixture.team_scheme);
    let patch = format!("/api/v4/schemes/{}/patch", fixture.team_scheme);

    // A well-formed body reaches the licence test: 501, with each route's own error id.
    let (status, body) = send(
        reqwest::Method::POST,
        "/api/v4/schemes",
        br#"{"name":"mmrs-would-be","display_name":"x","scope":"team"}"#,
    )
    .await;
    assert_eq!(status, 501);
    let body: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(body["id"], "api.scheme.create_scheme.license.error");

    let (status, body) = send(reqwest::Method::PUT, &patch, br#"{"display_name":"x"}"#).await;
    assert_eq!(status, 501);
    let body: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(body["id"], "api.scheme.patch_scheme.license.error");

    let (status, body) = send(reqwest::Method::DELETE, &scheme, b"").await;
    assert_eq!(status, 501);
    let body: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(
        body["id"], "api.scheme.delete_scheme.license.error",
        "and the scheme is still there afterwards, because nothing ran"
    );

    // **A malformed body is a 400, because the decode happens first.** Reversing the two would
    // answer 501 here, which is a different contract for any client that retries on 400.
    let (status, body) = send(reqwest::Method::POST, "/api/v4/schemes", b"{").await;
    assert_eq!(
        status, 400,
        "the body is decoded before the licence is read"
    );
    let body: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(body["id"], "api.context.invalid_body_param.app_error");

    let (status, _) = send(reqwest::Method::PUT, &patch, b"{").await;
    assert_eq!(
        status, 400,
        "the patch body is decoded before the licence too"
    );

    // And a bad id is a 400 ahead of everything, on both parameterised routes.
    let (status, _) = send(reqwest::Method::DELETE, "/api/v4/schemes/short", b"").await;
    assert_eq!(status, 400);
    let (status, _) = send(reqwest::Method::PUT, "/api/v4/schemes/short/patch", b"{}").await;
    assert_eq!(status, 400);

    // The delete really did nothing.
    let ((go_status, _), _) = fetch_both_raw(&client, &token, &scheme).await;
    assert_eq!(go_status, 200, "the 501 must not have deleted anything");
}

/// A plain user is refused every read, with each route's own permission.
#[tokio::test]
async fn a_plain_user_is_refused_every_read() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let Some(fixture) = fixture(&client, &admin).await else {
        return;
    };
    let team = common::create_team(&client, &admin, "schemeperm").await;
    let plain = common::create_plain_user(&client, &admin, &team, "schemeperm").await;

    for path in [
        "/api/v4/schemes".to_owned(),
        format!("/api/v4/schemes/{}", fixture.team_scheme),
        format!("/api/v4/schemes/{}/teams", fixture.team_scheme),
        format!("/api/v4/schemes/{}/channels", fixture.channel_scheme),
    ] {
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &plain.token, &path).await;
        assert_eq!(go_status, 403, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
        let body = common::assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(body["id"], "api.context.permissions.app_error");
    }

    common::delete_plain_user(&client, &admin, &plain.id).await;
}

/// Registering these must not turn another method on any of their paths into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for (method, path) in [
        (reqwest::Method::PUT, "/api/v4/schemes"),
        (
            reqwest::Method::PATCH,
            "/api/v4/schemes/zzzzzzzzzzzzzzzzzzzzzzzzzz",
        ),
        (
            reqwest::Method::POST,
            "/api/v4/schemes/zzzzzzzzzzzzzzzzzzzzzzzzzz/teams",
        ),
        (
            reqwest::Method::DELETE,
            "/api/v4/schemes/zzzzzzzzzzzzzzzzzzzzzzzzzz/channels",
        ),
    ] {
        let ours = client
            .request(method.clone(), format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("we answer");
        assert_eq!(
            ours.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{method} {path} must be forwarded"
        );
    }
}
