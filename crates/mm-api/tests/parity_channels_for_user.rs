//! Cross-server parity for `GET /api/v4/users/{user_id}/channels` — the route the webapp calls
//! on load as `/users/me/channels?include_deleted=…&last_delete_at=…`.
//!
//! Go **streams** this one: `[`, then each 100-channel page element by element through
//! `json.NewEncoder`, then `]`. The byte layout that implies — `}\n,{` between elements, no
//! newline after `]`, and a 200 whose body is `[` plus an error when there are no channels —
//! is what these tests pin against the running Go server. The list spans every team, so the
//! fixture user's list moves under every other suite; the tests that read it go through
//! `fetch_both_stable`, and the ones that need an exact total use a fresh plain user.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_channels_for_user
//! ```

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_channel, delete_plain_user, fetch_both_raw, fetch_both_stable, go_minted_token,
    logged_in_user_id, post_message, purge_api_fixtures, stack_enabled,
};

/// Create a team through Go's API and return its id. The creator joins automatically and gets
/// `town-square` and `off-topic`.
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

/// Go's `DELETE /teams/{id}` **archives** the team; `purge_api_fixtures` removes the row.
async fn archive_team(client: &reqwest::Client, token: &str, team_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/teams/{team_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "archiving the team failed: {}",
        response.text().await.unwrap_or_default()
    );
}

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

/// A user in **no team** — `create_plain_user` joins one, which is two channels too many for
/// the zero-channel case. Cleaned up by `delete_plain_user` / the purge like the others.
async fn create_teamless_user(client: &reqwest::Client, token: &str, tag: &str) -> String {
    let username = format!("mmrsplain{tag}");
    let response = client
        .post(format!("{GO}/api/v4/users"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "email": format!("{username}@mmrs.invalid"),
            "username": username,
            "password": "Mmrs-Plain-1234",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the teamless user failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the user decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Plant `count` open channels with `user_id` as a member, straight through the shared
/// database. Creating a hundred channels over REST is a hundred round trips per case and
/// archives rather than removes on cleanup; these carry the `mmrs-parity-` name prefix the
/// purge keys on and ids that sort among real ones. Every column Go scans is non-NULL.
async fn plant_member_channels(pool: &sqlx::PgPool, team_id: &str, user_id: &str, count: usize) {
    for i in 0..count {
        let id = format!("mmrsparitypage{i:0>12}");
        sqlx::query(
            "INSERT INTO channels (id, createat, updateat, deleteat, teamid, type, displayname,
                                   name, header, purpose, lastpostat, totalmsgcount,
                                   extraupdateat, creatorid, totalmsgcountroot, lastrootpostat)
             VALUES ($1, 1700000000000, 1700000000000, 0, $2, 'O', $3, $4, '', '', 0, 0, 0, '', 0, 0)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&id)
        .bind(team_id)
        .bind(format!("page {i}"))
        .bind(format!("mmrs-parity-page-{i}"))
        .execute(pool)
        .await
        .expect("inserts the channel");
        sqlx::query(
            "INSERT INTO channelmembers (channelid, userid, roles, notifyprops, schemeuser,
                                         lastviewedat, msgcount, mentioncount, lastupdateat)
             VALUES ($1, $2, '', '{}'::jsonb, true, 0, 0, 0, 0)
             ON CONFLICT DO NOTHING",
        )
        .bind(&id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("inserts the membership");
    }
}

async fn unplant_member_channels(pool: &sqlx::PgPool) {
    for statement in [
        "DELETE FROM channelmembers WHERE channelid LIKE 'mmrsparitypage%'",
        "DELETE FROM channels WHERE id LIKE 'mmrsparitypage%'",
    ] {
        let _ = sqlx::query(statement).execute(pool).await;
    }
}

fn ids_in(body: &[u8]) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_slice(body).expect("decodes");
    parsed
        .as_array()
        .expect("an array")
        .iter()
        .map(|c| c["id"].as_str().expect("an id").to_owned())
        .collect()
}

/// The streamed layout, beyond "the bytes agree": element separators are `}\n,{`, the last
/// element's newline precedes a bare `]`, and nothing follows it.
fn assert_streamed_layout(body: &[u8], path: &str) {
    let text = String::from_utf8_lossy(body);
    assert!(text.starts_with("[{"), "{path}: opens on the first element");
    assert!(
        text.ends_with("}\n]"),
        "{path}: `Encode`'s newline then `]`, and no newline after it: {:?}",
        &text[text.len().saturating_sub(20)..]
    );
    let elements = ids_in(body).len();
    assert_eq!(
        text.matches("}\n,{").count(),
        elements - 1,
        "{path}: every separator is newline-comma"
    );
}

/// The list, byte for byte, through `me` and through the explicit id: **id order** (the
/// streaming keyset's order, not the sibling route's display-name order), spanning a second
/// team, with a private channel, a DM, differing counters and a header `~mention` so
/// `FillInChannelsProps` has a prop to build.
#[tokio::test]
async fn the_list_is_byte_identical_in_id_order() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "allteam").await;
    let other_team = create_team(&client, &token, "allteam2").await;

    let zeta = create_named_channel(&client, &token, &team_id, "allzeta", "zeta all", "O").await;
    let alpha = create_named_channel(&client, &token, &team_id, "allalpha", "Alpha all", "O").await;
    let private =
        create_named_channel(&client, &token, &team_id, "allpriv", "Mid private", "P").await;
    let elsewhere =
        create_named_channel(&client, &token, &other_team, "allother", "Other team", "O").await;
    let root = post_message(&client, &token, &zeta, "a root post", None).await;
    post_message(&client, &token, &zeta, "a reply", Some(&root)).await;
    set_channel_header(
        &client,
        &token,
        &alpha,
        "See ~mmrs-parity-allzeta and ~mmrs-parity-allpriv",
    )
    .await;

    let other = create_plain_user(&client, &token, &team_id, "alldm").await;
    let dm = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([logged_in_user_id(), other.id]))
        .send()
        .await
        .expect("Go answers");
    assert!(dm.status().is_success());

    let me = logged_in_user_id();
    for path in [
        "/api/v4/users/me/channels".to_owned(),
        format!("/api/v4/users/{me}/channels"),
    ] {
        let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "{path}: the two servers must agree byte for byte"
        );
        assert_streamed_layout(&rs_body, &path);

        let ids = ids_in(&rs_body);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "ORDER BY Channels.Id");
        for (id, what) in [
            (&private, "the private channel"),
            (&elsewhere, "a channel in another team"),
        ] {
            assert!(ids.contains(id), "{what} is in the member's list");
        }

        let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
        let list = parsed.as_array().expect("an array");
        assert!(
            list.iter().any(|c| c["type"] == "D" && c["team_id"] == ""),
            "the DM is listed"
        );
        let zeta_row = list
            .iter()
            .find(|c| c["id"] == zeta.as_str())
            .expect("zeta");
        assert_ne!(
            zeta_row["total_msg_count"], zeta_row["total_msg_count_root"],
            "equal counters could not catch a swapped column"
        );
        let alpha_row = list
            .iter()
            .find(|c| c["id"] == alpha.as_str())
            .expect("alpha");
        assert_eq!(
            alpha_row["props"]["channel_mentions"],
            serde_json::json!({ "mmrs-parity-allzeta": { "display_name": "zeta all" } }),
            "FillInChannelsProps ran, and only the open mention rendered"
        );
    }

    for id in [&zeta, &alpha, &private, &elsewhere] {
        delete_channel(&client, &token, id).await;
    }
    delete_plain_user(&client, &token, &other.id).await;
}

/// The page loop, on a plain user whose total is known exactly: a team's two defaults plus
/// planted channels to make **100** (an exact multiple — the loop ends on the swallowed
/// `not_found` for the empty second page), then **101** (a second page of one, joined with a
/// comma), then **200** (two full pages and a third empty one).
#[tokio::test]
async fn the_pages_join_seamlessly_and_an_exact_multiple_terminates_cleanly() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL is needed to plant the channels");
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connects");

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "pageteam").await;
    let plain = create_plain_user(&client, &token, &team_id, "pages").await;
    let path = format!("/api/v4/users/{}/channels", plain.id);

    let (go_body, _) = fetch_both_stable(&client, &token, &path).await;
    let baseline = ids_in(&go_body).len();
    assert_eq!(baseline, 2, "town-square and off-topic");

    unplant_member_channels(&pool).await;
    for total in [100_usize, 101, 200] {
        plant_member_channels(&pool, &team_id, &plain.id, total - baseline).await;

        let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "{total} channels: the two servers must agree byte for byte"
        );
        assert_streamed_layout(&rs_body, &path);
        assert_eq!(
            ids_in(&rs_body).len(),
            total,
            "{total}: every page reached the wire"
        );
    }

    unplant_member_channels(&pool).await;
    delete_plain_user(&client, &token, &plain.id).await;
}

/// Zero channels is **not a 404 and not `[]`**: Go has already written `200` and `[` when the
/// store's `not_found` arrives, so the body is `[` followed by the error JSON — the status in
/// the body says 404, the status line says 200, and the whole is not JSON.
#[tokio::test]
async fn zero_channels_is_a_200_whose_body_is_a_bracket_and_the_error() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let nobody = create_teamless_user(&client, &token, "nochannels").await;

    let path = format!("/api/v4/users/{nobody}/channels");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    delete_plain_user(&client, &token, &nobody).await;

    assert_eq!(go_status, 200, "Go committed the status before the query");
    assert_eq!(rs_status, 200);
    assert_eq!(go_body.first(), Some(&b'['));
    assert_eq!(rs_body.first(), Some(&b'['));
    assert!(serde_json::from_slice::<serde_json::Value>(&go_body).is_err());
    let go = assert_error_bodies_match_except_known_gaps(&go_body[1..], &rs_body[1..], "zero");
    assert_eq!(
        go["id"].as_str(),
        Some("app.channel.get_channels.not_found.app_error")
    );
    assert_eq!(go["status_code"], 404);
}

/// The deletion filters, on the **team** as well as the channel: default hides an archived
/// channel and every channel of an archived team; `include_deleted=true` shows both;
/// `last_delete_at` keeps each only when archived at or after it (`>=`). A negative value is
/// this route's one 400 — and it is a real 400, because it is raised before the `[`.
#[tokio::test]
async fn include_deleted_and_last_delete_at_filter_channels_and_their_teams() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "delallteam").await;
    let archived =
        create_named_channel(&client, &token, &team_id, "delallarch", "archived one", "O").await;
    delete_channel(&client, &token, &archived).await;
    let doomed_team = create_team(&client, &token, "doomedteam").await;
    let in_doomed = create_named_channel(
        &client,
        &token,
        &doomed_team,
        "delalldoomed",
        "in doomed",
        "O",
    )
    .await;
    archive_team(&client, &token, &doomed_team).await;

    let base = "/api/v4/users/me/channels";
    let listed = |body: &[u8], id: &str| ids_in(body).iter().any(|c| c == id);

    let (go_body, rs_body) = fetch_both_stable(&client, &token, base).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert!(
        !listed(&rs_body, &archived),
        "archived channels are hidden by default"
    );
    assert!(
        !listed(&rs_body, &in_doomed),
        "a living channel of an archived team is hidden by default"
    );

    let path = format!("{base}?include_deleted=true");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert!(
        listed(&rs_body, &archived),
        "include_deleted shows the archived channel"
    );
    assert!(
        listed(&rs_body, &in_doomed),
        "include_deleted shows the archived team's channel"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    let delete_at = parsed
        .as_array()
        .expect("an array")
        .iter()
        .find(|c| c["id"] == archived.as_str())
        .and_then(|c| c["delete_at"].as_i64())
        .expect("the archived row carries its delete_at");
    assert!(delete_at > 0);
    let team_delete_at: i64 = {
        let response = client
            .get(format!("{GO}/api/v4/teams/{doomed_team}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        let team: serde_json::Value = response.json().await.expect("the team decodes");
        team["delete_at"].as_i64().expect("archived")
    };
    assert!(
        team_delete_at > delete_at,
        "the team was archived after the channel"
    );

    for (last_delete_at, channel_expected, team_channel_expected) in [
        (delete_at, true, true),
        (delete_at + 1, false, true),
        (team_delete_at, false, true),
        (team_delete_at + 1, false, false),
        (1, true, true),
    ] {
        let path = format!("{base}?include_deleted=true&last_delete_at={last_delete_at}");
        let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "{path}"
        );
        assert_eq!(
            listed(&rs_body, &archived),
            channel_expected,
            "{path}: Channels.DeleteAt >= last_delete_at"
        );
        assert_eq!(
            listed(&rs_body, &in_doomed),
            team_channel_expected,
            "{path}: Teams.DeleteAt >= last_delete_at"
        );
    }

    // Without include_deleted, last_delete_at is ignored entirely.
    let path = format!("{base}?last_delete_at=1");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert!(!listed(&rs_body, &archived));
    assert!(!listed(&rs_body, &in_doomed));

    let path = format!("{base}?last_delete_at=-1");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, 400);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "negative");
    assert_eq!(
        go["id"].as_str(),
        Some("api.context.invalid_url_param.app_error")
    );
}

/// The one gate: a plain user asking about **another user** is refused with
/// `edit_other_users` — before the query string is read, so `last_delete_at=-1` is still the
/// 403 — and a malformed id is the 400.
#[tokio::test]
async fn another_user_is_a_403_and_a_malformed_id_a_400() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (home_team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let plain = create_plain_user(&client, &token, &home_team, "allgate").await;

    let admin = logged_in_user_id();
    for path in [
        format!("/api/v4/users/{admin}/channels"),
        format!("/api/v4/users/{admin}/channels?last_delete_at=-1"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &plain.token, &path).await;
        assert_eq!(go_status, 403, "{path}");
        assert_eq!(rs_status, 403, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));
    }
    delete_plain_user(&client, &token, &plain.id).await;

    let path = "/api/v4/users/short/channels";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, 400);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(
        go["id"].as_str(),
        Some("api.context.invalid_url_param.app_error")
    );
}

/// Go's deeper routes under the same prefix that are not migrated —
/// `…/channels/{channel_id}/posts/unread` — must still reach Go, and a GET to
/// `…/channels/{channel_id}` itself, which Go has no route for, must be Go's 404 and not ours.
#[tokio::test]
async fn deeper_sibling_routes_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel_id) = common::a_team_and_channel_the_user_is_in(&client, &token).await;

    for (suffix, expected_status) in [("/posts/unread", 200), ("", 404)] {
        let path = format!("/api/v4/users/me/channels/{channel_id}{suffix}");
        let response = client
            .get(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the Rust server answers");
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{path}: not migrated, must be forwarded"
        );
        assert_eq!(response.status().as_u16(), expected_status, "{path}");
    }
}
