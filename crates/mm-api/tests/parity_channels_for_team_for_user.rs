//! Cross-server parity for `GET /api/v4/users/{user_id}/teams/{team_id}/channels` — the route
//! the webapp calls on every team load.
//!
//! Everything here runs against a **fresh team** created per test, because the shared fixture
//! team is written to by every other parity suite (and by the other worktrees' suites) and a
//! list is only comparable when its membership holds still. DMs are teamless and therefore
//! appear in every team's list, so a DM another suite creates for the fixture user can still
//! move the admin's list under a test; `fetch_both_stable` absorbs that.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_channels_for_team_for_user
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

/// Create a channel with an explicit display name, so the `ORDER BY DisplayName` the route is
/// bound to can be arranged rather than hoped for.
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

/// `(status, etag, body)` from one server.
async fn get_with_etag(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    if_none_match: Option<&str>,
) -> (u16, Option<String>, Vec<u8>) {
    let mut request = client
        .get(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(etag) = if_none_match {
        request = request.header("If-None-Match", etag);
    }
    let response = request.send().await.expect("the server answers");
    let status = response.status().as_u16();
    let etag = response
        .headers()
        .get("ETag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if base == RUST {
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust"),
            "{path} must be served by the Rust side"
        );
    }
    (
        status,
        etag,
        response.bytes().await.expect("body reads").to_vec(),
    )
}

/// Go's etag and ours for `path`, read Go-ours-Go and retried until the two Go reads agree —
/// the list can move under a test (another suite opening a DM with the fixture user adds a
/// teamless row to every team's list), and a moved list is not a divergence.
async fn etags_when_stable(
    client: &reqwest::Client,
    token: &str,
    path: &str,
) -> (Option<String>, Option<String>) {
    for attempt in 1..=8_u64 {
        let (_, before, _) = get_with_etag(client, GO, token, path, None).await;
        let (_, ours, _) = get_with_etag(client, RUST, token, path, None).await;
        let (_, after, _) = get_with_etag(client, GO, token, path, None).await;
        if before == after {
            return (before, ours);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50 * attempt)).await;
    }
    panic!("{path}: Go's etag never settled");
}

/// The list, byte for byte, through `me` and through the explicit id: display-name order with
/// the fixture arranged so creation order and name order disagree, a private channel and a DM
/// in the list, a root post and a reply so the two counters differ, and a header `~mention` so
/// `FillInChannelsProps` has a prop to build. The `ETag` header must agree too.
#[tokio::test]
async fn the_list_is_byte_identical_in_display_name_order() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "listteam").await;

    // Created z-then-a so heap order and display-name order disagree.
    let zeta = create_named_channel(&client, &token, &team_id, "listzeta", "zeta list", "O").await;
    let alpha =
        create_named_channel(&client, &token, &team_id, "listalpha", "Alpha list", "O").await;
    let private =
        create_named_channel(&client, &token, &team_id, "listpriv", "Mid private", "P").await;
    let root = post_message(&client, &token, &zeta, "a root post", None).await;
    post_message(&client, &token, &zeta, "a reply", Some(&root)).await;
    set_channel_header(
        &client,
        &token,
        &alpha,
        "See ~mmrs-parity-listzeta and ~mmrs-parity-listpriv",
    )
    .await;

    let other = create_plain_user(&client, &token, &team_id, "listdm").await;
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
        format!("/api/v4/users/me/teams/{team_id}/channels"),
        format!("/api/v4/users/{me}/teams/{team_id}/channels"),
    ] {
        let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "{path}: the two servers must agree byte for byte"
        );
        assert_eq!(rs_body.last(), Some(&b'\n'), "encoder newline ([D-086])");

        let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
        let list = parsed.as_array().expect("an array");
        let display_names: Vec<&str> = list
            .iter()
            .filter_map(|c| c["display_name"].as_str())
            .collect();
        let mut sorted = display_names.clone();
        sorted.sort_unstable();
        assert_eq!(display_names, sorted, "ORDER BY DisplayName");
        assert!(
            list.iter().any(|c| c["id"] == private.as_str()),
            "the private channel is in the member's list"
        );
        assert!(
            list.iter().any(|c| c["type"] == "D" && c["team_id"] == ""),
            "the DM — teamless — appears in this team's list"
        );
        let zeta_row = list
            .iter()
            .find(|c| c["id"] == zeta.as_str())
            .expect("zeta is listed");
        assert_ne!(
            zeta_row["total_msg_count"], zeta_row["total_msg_count_root"],
            "equal counters could not catch a swapped column"
        );
        let alpha_row = list
            .iter()
            .find(|c| c["id"] == alpha.as_str())
            .expect("alpha is listed");
        assert_eq!(
            alpha_row["props"]["channel_mentions"],
            serde_json::json!({ "mmrs-parity-listzeta": { "display_name": "zeta list" } }),
            "FillInChannelsProps ran, and only the open mention rendered"
        );

        let (go_etag, rs_etag) = etags_when_stable(&client, &token, &path).await;
        assert!(go_etag.is_some(), "Go sets ETag");
        assert_eq!(rs_etag, go_etag, "{path}: ChannelList.Etag");
    }

    for id in [&zeta, &alpha, &private] {
        delete_channel(&client, &token, id).await;
    }
    delete_plain_user(&client, &token, &other.id).await;
}

/// `HandleEtag`: the etag from one fetch, sent back as `If-None-Match`, is a 304 with the
/// `ETag` header and no body — from both servers, and across them (Go's etag satisfies ours).
#[tokio::test]
async fn a_matching_if_none_match_is_a_304() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "etagteam").await;
    let path = format!("/api/v4/users/me/teams/{team_id}/channels");

    for base in [GO, RUST] {
        // Re-read Go's etag per attempt: the list can move under the test (see the module
        // docs), and a 200 against a stale etag is then Go's answer too, not a divergence.
        let mut outcome = None;
        for attempt in 1..=8_u64 {
            let (status, go_etag, _) = get_with_etag(&client, GO, &token, &path, None).await;
            assert_eq!(status, 200);
            let go_etag = go_etag.expect("Go sets ETag");
            let (status, etag, body) =
                get_with_etag(&client, base, &token, &path, Some(&go_etag)).await;
            if status == 304 {
                outcome = Some((go_etag, etag, body));
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50 * attempt)).await;
        }
        let (go_etag, etag, body) = outcome.unwrap_or_else(|| panic!("{base}: never a 304"));
        assert_eq!(etag.as_deref(), Some(go_etag.as_str()), "{base}");
        assert!(body.is_empty(), "{base}: a 304 has no body");

        let (status, _, body) =
            get_with_etag(&client, base, &token, &path, Some("10.0.0.stale.0.0.0")).await;
        assert_eq!(status, 200, "{base}: a stale etag is a full answer");
        assert!(!body.is_empty());
    }
}

/// The three deletion filters: default excludes the archived channel, `include_deleted=true`
/// includes it, and `last_delete_at` keeps it only when it was archived **at or after** that
/// instant (`>=`, so exactly its `delete_at` keeps it and one more drops it). A negative value
/// is this route's one 400.
#[tokio::test]
async fn include_deleted_and_last_delete_at_select_the_archived_channels() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let team_id = create_team(&client, &token, "delteam").await;
    let archived =
        create_named_channel(&client, &token, &team_id, "delarch", "archived one", "O").await;
    delete_channel(&client, &token, &archived).await;

    let base = format!("/api/v4/users/me/teams/{team_id}/channels");
    let listed = |body: &[u8]| -> bool {
        let parsed: serde_json::Value = serde_json::from_slice(body).expect("decodes");
        parsed
            .as_array()
            .expect("an array")
            .iter()
            .any(|c| c["id"] == archived.as_str())
    };

    let (go_body, rs_body) = fetch_both_stable(&client, &token, &base).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert!(!listed(&rs_body), "archived channels are hidden by default");

    let path = format!("{base}?include_deleted=true");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert!(listed(&rs_body), "include_deleted shows it");
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    let delete_at = parsed
        .as_array()
        .expect("an array")
        .iter()
        .find(|c| c["id"] == archived.as_str())
        .and_then(|c| c["delete_at"].as_i64())
        .expect("the archived row carries its delete_at");
    assert!(delete_at > 0);

    for (last_delete_at, expected) in [(delete_at, true), (delete_at + 1, false), (1, true)] {
        let path = format!("{base}?include_deleted=true&last_delete_at={last_delete_at}");
        let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "{path}"
        );
        assert_eq!(
            listed(&rs_body),
            expected,
            "{path}: DeleteAt >= last_delete_at"
        );
    }

    // Without include_deleted, last_delete_at is ignored entirely.
    let path = format!("{base}?last_delete_at=1");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert!(!listed(&rs_body));

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

/// Zero channels is a **404**, not `[]`: the admin passes both gates for a team the target user
/// never joined, and the store's `ErrNotFound` reaches the wire.
#[tokio::test]
async fn a_user_with_no_channels_in_the_team_is_a_404() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (home_team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let stranger = create_plain_user(&client, &token, &home_team, "liststranger").await;
    let team_id = create_team(&client, &token, "emptyteam").await;

    let path = format!("/api/v4/users/{}/teams/{team_id}/channels", stranger.id);
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    delete_plain_user(&client, &token, &stranger.id).await;

    assert_eq!(go_status, 404);
    assert_eq!(rs_status, 404);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "no channels");
    assert_eq!(
        go["id"].as_str(),
        Some("app.channel.get_channels.not_found.app_error")
    );
}

/// The two gates with an actor who can be refused: asking about **another user** fails the
/// user gate; asking about oneself in a team one is **not a member of** fails the team gate.
/// Both are the same 403 body.
#[tokio::test]
async fn a_plain_user_is_refused_by_both_gates() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (home_team, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let plain = create_plain_user(&client, &token, &home_team, "listgates").await;
    let foreign_team = create_team(&client, &token, "foreignteam").await;

    let admin = logged_in_user_id();
    for (path, why) in [
        (
            format!("/api/v4/users/{admin}/teams/{home_team}/channels"),
            "another user: edit_other_users",
        ),
        (
            format!("/api/v4/users/me/teams/{foreign_team}/channels"),
            "a team not joined: view_team",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &plain.token, &path).await;
        assert_eq!(go_status, 403, "{why}");
        assert_eq!(rs_status, 403, "{why}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, why);
        assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));
    }

    delete_plain_user(&client, &token, &plain.id).await;
}

/// A malformed id in either segment is `invalid_url_param` — and Go's `RequireUserId` handles
/// `me` before validating, so the literal never 400s.
#[tokio::test]
async fn malformed_ids_are_400s() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;

    for path in [
        format!("/api/v4/users/short/teams/{team_id}/channels"),
        "/api/v4/users/me/teams/short/channels".to_owned(),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, 400, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(
            go["id"].as_str(),
            Some("api.context.invalid_url_param.app_error")
        );
    }
}

/// Go's deeper sibling under the same prefix — `…/channels/categories` — is not migrated and
/// must still reach Go. (`…/channels/members` was in this list until it was served; see
/// `parity_channel_members_for_team_for_user.rs`.)
#[tokio::test]
async fn deeper_sibling_routes_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;

    for suffix in ["categories"] {
        let path = format!("/api/v4/users/me/teams/{team_id}/channels/{suffix}");
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
            "{suffix}: not migrated, must be forwarded"
        );
        assert_eq!(response.status().as_u16(), 200, "{suffix}");
    }
}
