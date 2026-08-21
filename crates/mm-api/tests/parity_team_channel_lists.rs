//! Cross-server parity for the three paged channel lists a team owns:
//!
//! - `GET /api/v4/teams/{team_id}/channels` — `getPublicChannelsForTeam`, the "Browse channels"
//!   list the webapp actually calls
//! - `GET /api/v4/teams/{team_id}/channels/private` — `getPrivateChannelsForTeam`
//! - `GET /api/v4/teams/{team_id}/channels/deleted` — `getDeletedChannelsForTeam`
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_team_channel_lists
//! ```
//!
//! Every test builds a **fresh team**, because the shared fixture team is written to by the other
//! parity suites and by the other worktrees, and a list is only comparable while it holds still.
//!
//! # The one thing these three routes do not share
//!
//! Their gates. `/channels` and `/channels/deleted` ask `list_team_channels` **on the team**, so
//! an ordinary member passes; `/channels/private` asks `manage_system` **on the system**, with no
//! team argument, so a team admin is refused. The suite runs a non-admin actor against all three
//! for exactly that reason — the fixture user is a `system_admin` and cannot tell the three
//! constants apart.
//!
//! `/channels/deleted` then asks `manage_system` a *second* time, and that answer is not a gate:
//! it becomes `skipTeamMembershipCheck`, which widens the result. So the route needs two actors
//! to be pinned at all, and the plain user is the only one who can see the narrow answer.

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_channel, delete_plain_user, fetch_both_raw, fetch_both_stable, go_minted_token,
    purge_api_fixtures, stack_enabled,
};

async fn create_team(client: &reqwest::Client, token: &str, tag: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/teams"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture team failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the team decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Create a channel with an explicit display name and type, as whoever holds `token` — which
/// decides who ends up a member, and therefore whether the browse list is showing channels the
/// caller has *not* joined.
async fn create_named_channel(
    client: &reqwest::Client,
    token: &str,
    team_id: &str,
    tag: &str,
    display_name: &str,
    channel_type: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("mmrs-parity-{tag}"),
            "display_name": display_name,
            "type": channel_type,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn set_channel_header(client: &reqwest::Client, token: &str, channel_id: &str, header: &str) {
    let response = client
        .put(format!("{GO}/api/v4/channels/{channel_id}/patch"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "header": header }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "patching the header failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Which server answered, plus the `ETag` header — the second of which is a claim about these
/// routes in its own right: Go computes no etag for any of the three.
async fn served_by_and_etag(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
) -> (u16, Option<String>, Option<String>) {
    let response = client
        .get(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    (
        response.status().as_u16(),
        header("x-mmrs-served-by"),
        header("ETag"),
    )
}

fn ids_of(body: &[u8]) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_slice(body).expect("the list decodes");
    parsed
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|c| c["id"].as_str().map(str::to_owned))
        .collect()
}

fn display_names_of(body: &[u8]) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_slice(body).expect("the list decodes");
    parsed
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|c| c["display_name"].as_str().map(str::to_owned))
        .collect()
}

/// The browse list: every living public channel of the team **whether or not the caller joined
/// it**, in display-name order, with the archived one and the private one absent, and
/// `FillInChannelsProps` having resolved the `~mention` in a header.
#[tokio::test]
async fn the_public_list_matches_go_and_lists_unjoined_channels() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "pubteam").await;
    let other = create_plain_user(&client, &token, &team_id, "pubuser").await;

    // Created z-then-a so heap order and display-name order disagree.
    let zeta = create_named_channel(&client, &token, &team_id, "pubzeta", "zeta pub", "O").await;
    let alpha = create_named_channel(&client, &token, &team_id, "pubalpha", "Alpha pub", "O").await;
    // Created by the *other* user, so the admin is not a member of it.
    let unjoined = create_named_channel(
        &client,
        &other.token,
        &team_id,
        "pubunjoined",
        "Beta unjoined",
        "O",
    )
    .await;
    let private = create_named_channel(&client, &token, &team_id, "pubpriv", "Mid priv", "P").await;
    let archived =
        create_named_channel(&client, &token, &team_id, "pubarch", "Delta arch", "O").await;
    delete_channel(&client, &token, &archived).await;
    set_channel_header(&client, &token, &alpha, "See ~mmrs-parity-pubzeta").await;

    let path = format!("/api/v4/teams/{team_id}/channels");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "{path}: the two servers must agree byte for byte"
    );
    assert_eq!(rs_body.last(), Some(&b'\n'), "encoder newline ([D-086])");

    let ids = ids_of(&rs_body);
    let names = display_names_of(&rs_body);
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "ORDER BY DisplayName");
    assert!(ids.contains(&zeta), "a joined public channel is listed");
    assert!(
        ids.contains(&unjoined),
        "and so is one the caller never joined — this is browse, not the sidebar"
    );
    assert!(!ids.contains(&private), "a private channel is not public");
    assert!(!ids.contains(&archived), "nor is an archived one");

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    let alpha_row = parsed
        .as_array()
        .expect("an array")
        .iter()
        .find(|c| c["id"] == alpha.as_str())
        .expect("alpha is listed")
        .clone();
    assert_eq!(
        alpha_row["props"]["channel_mentions"],
        serde_json::json!({ "mmrs-parity-pubzeta": { "display_name": "zeta pub" } }),
        "FillInChannelsProps ran"
    );

    // No etag on any of the three, unlike `getChannelsForTeamForUser` one path segment over.
    for base in [GO, RUST] {
        let (status, _, etag) = served_by_and_etag(&client, base, &token, &path).await;
        assert_eq!(status, 200);
        assert_eq!(etag, None, "{base}: this handler computes no etag");
    }

    for id in [&zeta, &alpha, &unjoined, &private] {
        delete_channel(&client, &token, id).await;
    }
    delete_plain_user(&client, &token, &other.id).await;
}

/// Paging is `OFFSET page * per_page`, and both ends of it are `200 []` rather than an error:
/// a page past the end, and `per_page=0`, which is a real `LIMIT 0` here — the opposite of
/// `getChannelMembers`, where the store's `Limit > 0` guard turns the same zero into "no limit".
#[tokio::test]
async fn public_paging_walks_by_offset_and_runs_out_on_an_empty_list() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "pageteam").await;
    // Plus the town-square and off-topic every new team gets: five in all.
    let made: Vec<String> = {
        let mut made = Vec::new();
        for (tag, display) in [
            ("pagea", "aa page"),
            ("pageb", "bb page"),
            ("pagec", "cc page"),
        ] {
            made.push(create_named_channel(&client, &token, &team_id, tag, display, "O").await);
        }
        made
    };

    let base_path = format!("/api/v4/teams/{team_id}/channels");
    let (go_all, rs_all) = fetch_both_stable(&client, &token, &base_path).await;
    assert_eq!(rs_all, go_all, "the unpaged list must agree first");
    let all = ids_of(&rs_all);
    assert_eq!(all.len(), 5, "three made plus town-square and off-topic");

    // Bounded: an off-by-one in the offset arithmetic must fail here, not spin. `getChannelsForUser`
    // once had a mutant that made a page walk return the same row for ever.
    let mut walked: Vec<String> = Vec::new();
    for page in 0..8_i64 {
        let path = format!("{base_path}?page={page}&per_page=2");
        let (go_page, rs_page) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_page),
            String::from_utf8_lossy(&go_page),
            "{path}"
        );
        let ids = ids_of(&rs_page);
        if ids.is_empty() {
            break;
        }
        walked.extend(ids);
    }
    assert_eq!(walked, all, "the pages tile the list exactly once each");

    for (query, why) in [
        ("?page=99&per_page=60", "a page past the end"),
        ("?per_page=0", "LIMIT 0"),
    ] {
        let path = format!("{base_path}{query}");
        let (go, rs) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(rs, b"[]\n".to_vec(), "{why}: {path}");
        assert_eq!(go, rs, "{why}: {path}");
    }

    // Garbage and negatives fall to the defaults rather than 400ing — `strconv.Atoi`'s error is
    // discarded in `ParamsFromRequest`.
    for query in [
        "?page=-1&per_page=-3",
        "?page=abc&per_page=abc",
        "?page=&per_page=",
    ] {
        let path = format!("{base_path}{query}");
        let (go, rs) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(rs, go, "{path}");
        assert_eq!(
            ids_of(&rs),
            all,
            "{path}: falls back to page 0, per_page 60"
        );
    }

    for id in &made {
        delete_channel(&client, &token, id).await;
    }
}

/// `page * per_page` overflows `int64` and **wraps**, so a large enough page reaches the store as
/// a negative offset and Postgres refuses it. Both servers answer 500 with the store's own error
/// id — not `200 []`, which is what clamping the page would have produced.
#[tokio::test]
async fn an_overflowing_page_is_the_same_500_on_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "ovflteam").await;

    let path = format!("/api/v4/teams/{team_id}/channels?page=9223372036854775807&per_page=200");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;

    assert_eq!(go_status, 500, "Go: {}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        500,
        "ours: {}",
        String::from_utf8_lossy(&rs_body)
    );
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "app.channel.get_public_channels.get.app_error");
}

/// The private list is `manage_system`-gated: the same request that a system admin answers with a
/// list is a 403 for an ordinary member of the very same team. And it lists living private
/// channels only — the archived one is the `deleted` route's business.
#[tokio::test]
async fn the_private_list_is_system_gated_and_excludes_archived() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "privteam").await;
    let other = create_plain_user(&client, &token, &team_id, "privuser").await;

    let living = create_named_channel(&client, &token, &team_id, "privl", "aa priv", "P").await;
    let public = create_named_channel(&client, &token, &team_id, "privpub", "bb pub", "O").await;
    let archived = create_named_channel(&client, &token, &team_id, "priva", "cc priv", "P").await;
    delete_channel(&client, &token, &archived).await;

    let path = format!("/api/v4/teams/{team_id}/channels/private");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "{path}"
    );
    assert_eq!(rs_body.last(), Some(&b'\n'));
    assert_eq!(
        ids_of(&rs_body),
        vec![living.clone()],
        "living private channels only"
    );

    // The actor who can be refused. A member of the team, holding `list_team_channels` — which
    // is enough for the two sibling routes and not for this one.
    let ((go_status, go_403), (rs_status, rs_403)) =
        fetch_both_raw(&client, &other.token, &path).await;
    assert_eq!(go_status, 403, "Go refuses a plain team member");
    assert_eq!(rs_status, 403, "and so must we");
    assert_error_bodies_match_except_known_gaps(&go_403, &rs_403, &path);

    // …and the same actor is *allowed* on the public list of the same team, which is what makes
    // the 403 above a statement about the permission rather than about the user.
    let public_path = format!("/api/v4/teams/{team_id}/channels");
    let ((go_status, _), (rs_status, _)) =
        fetch_both_raw(&client, &other.token, &public_path).await;
    assert_eq!((go_status, rs_status), (200, 200), "{public_path}");

    for id in [&living, &public] {
        delete_channel(&client, &token, id).await;
    }
    delete_plain_user(&client, &token, &other.id).await;
}

/// `skipTeamMembershipCheck` in both positions. The admin holds `manage_system` and sees every
/// archived channel of the team; the plain member sees the archived **public** one plus the
/// archived private one they are still a member of, and not the archived private one they never
/// joined.
#[tokio::test]
async fn the_deleted_list_widens_for_manage_system_and_narrows_otherwise() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "delteam").await;
    let other = create_plain_user(&client, &token, &team_id, "deluser").await;

    let pub_arch = create_named_channel(&client, &token, &team_id, "delpub", "aa pub", "O").await;
    let living = create_named_channel(&client, &token, &team_id, "delliving", "bb live", "O").await;
    // The plain user creates this one, so they are its only member.
    let priv_mine = create_named_channel(
        &client,
        &other.token,
        &team_id,
        "delprivmine",
        "cc priv mine",
        "P",
    )
    .await;
    let priv_theirs =
        create_named_channel(&client, &token, &team_id, "delprivnot", "dd priv not", "P").await;
    for id in [&pub_arch, &priv_mine, &priv_theirs] {
        delete_channel(&client, &token, id).await;
    }

    let path = format!("/api/v4/teams/{team_id}/channels/deleted");

    let (go_wide, rs_wide) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_wide),
        String::from_utf8_lossy(&go_wide),
        "{path} as the system admin"
    );
    assert_eq!(rs_wide.last(), Some(&b'\n'));
    let wide = ids_of(&rs_wide);
    for id in [&pub_arch, &priv_mine, &priv_theirs] {
        assert!(
            wide.contains(id),
            "manage_system sees every archived channel"
        );
    }
    assert!(!wide.contains(&living), "a living channel is not deleted");

    let (go_narrow, rs_narrow) = fetch_both_stable(&client, &other.token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_narrow),
        String::from_utf8_lossy(&go_narrow),
        "{path} as a plain member"
    );
    let narrow = ids_of(&rs_narrow);
    assert!(
        narrow.contains(&pub_arch),
        "an archived public channel needs no membership"
    );
    assert!(
        narrow.contains(&priv_mine),
        "an archived private channel does, and this one has it"
    );
    assert!(
        !narrow.contains(&priv_theirs),
        "and this one does not — the membership subquery is load-bearing"
    );

    delete_channel(&client, &token, &living).await;
    delete_plain_user(&client, &token, &other.id).await;
}

/// A malformed team id is a 400 from the handler; a segment outside gorilla's
/// `[A-Za-z0-9]+` class never reaches one and is forwarded so Go answers its own mux 404
/// ([D-150]). Asserted on all three routes, because each registers the parameter separately.
#[tokio::test]
async fn a_bad_team_id_400s_and_a_non_id_segment_is_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }

    let client = client();
    let token = go_minted_token(&client).await;

    for suffix in ["", "/private", "/deleted"] {
        let path = format!("/api/v4/teams/notanid/channels{suffix}");
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!((go_status, rs_status), (400, 400), "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");

        let hyphenated = format!("/api/v4/teams/bad-id/channels{suffix}");
        let (status, served_by, _) = served_by_and_etag(&client, RUST, &token, &hyphenated).await;
        assert_eq!(status, 404, "{hyphenated}");
        assert_eq!(
            served_by.as_deref(),
            Some("go"),
            "{hyphenated}: outside the mux charset, so Go answers its own 404"
        );
    }
}

/// The sibling literals under `/teams/{team_id}/channels/` must keep being forwarded exactly as
/// they were before these three routes existed.
///
/// This is a claim about the **router**, not about a handler: gorilla registers nine literals
/// under this prefix and we now serve three of them, so a registration that accidentally
/// swallowed the rest would be invisible to every other test in this file. Each assertion is
/// `x-mmrs-served-by: go`, which only the proxy sets.
#[tokio::test]
async fn the_sibling_literals_are_still_forwarded_to_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "sibteam").await;

    // GET siblings. `/recommended` and the two autocompletes are GET routes in Go;
    // `/managed_categories` is behind a feature flag and 404s when it is off — forwarded either
    // way, which is the whole point.
    for literal in [
        "recommended",
        "autocomplete?name=town",
        "search_autocomplete?name=town",
        "managed_categories",
    ] {
        let path = format!("/api/v4/teams/{team_id}/channels/{literal}");
        let (_, served_by, _) = served_by_and_etag(&client, RUST, &token, &path).await;
        assert_eq!(
            served_by.as_deref(),
            Some("go"),
            "{path} must stay forwarded"
        );
    }

    // POST-only siblings, plus a POST to a path we serve for GET: the method fallback in
    // `partially_migrated` has to forward, or migrating GET would have broken POST.
    for (literal, body) in [
        ("ids", serde_json::json!([])),
        ("search", serde_json::json!({ "term": "town" })),
        ("", serde_json::json!({})),
    ] {
        let path = if literal.is_empty() {
            format!("/api/v4/teams/{team_id}/channels")
        } else {
            format!("/api/v4/teams/{team_id}/channels/{literal}")
        };
        let response = client
            .post(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&body)
            .send()
            .await
            .expect("the server answers");
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "POST {path} must stay forwarded"
        );
    }

    // And the deeper by-name route, which we do serve, still is — proof that adding the two
    // literals above it did not change which route claims that path.
    let by_name = format!("/api/v4/teams/{team_id}/channels/name/town-square");
    let (status, served_by, _) = served_by_and_etag(&client, RUST, &token, &by_name).await;
    assert_eq!(status, 200, "{by_name}");
    assert_eq!(served_by.as_deref(), Some("rust"), "{by_name}");
}
