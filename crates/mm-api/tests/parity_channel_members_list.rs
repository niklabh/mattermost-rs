//! Cross-server parity for `GET /api/v4/channels/{channel_id}/members` — the first paginated
//! route, so the suite's centre of gravity is Go's pagination contract: garbage never 400s,
//! `per_page` clamps at 200, and `per_page=0` means **everything**, because the store's
//! `Limit > 0` is a guard rather than a clamp.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_channel_members_list
//! ```

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_plain_user, delete_channel, delete_plain_user, fetch_both_raw, fetch_both_stable,
    go_minted_token, purge_api_fixtures, stack_enabled,
};

/// A channel with the admin plus three plain members — four rows, enough to split across pages.
async fn members_fixture(
    client: &reqwest::Client,
    token: &str,
    tag: &str,
) -> (String, Vec<common::PlainUser>) {
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, token).await;
    let channel_id = create_channel(client, token, &team_id, tag).await;
    let mut users = Vec::new();
    for n in 1..=3 {
        let user = create_plain_user(client, token, &team_id, &format!("{tag}{n}")).await;
        common::add_user_to_channel(client, token, &channel_id, &user.id).await;
        users.push(user);
    }
    (channel_id, users)
}

async fn teardown(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    users: &[common::PlainUser],
) {
    delete_channel(client, token, channel_id).await;
    for user in users {
        delete_plain_user(client, token, &user.id).await;
    }
}

fn member_ids(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .expect("decodes")
        .as_array()
        .expect("an array")
        .iter()
        .map(|m| m["user_id"].as_str().expect("a user id").to_owned())
        .collect()
}

/// The default page, byte for byte — and the sanitiser's shape inside a list: every row's two
/// timestamps blank to `-1` except the caller's own, which keeps its values mid-list.
#[tokio::test]
async fn the_member_list_is_byte_identical_and_sanitised_around_the_caller() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (channel_id, users) = members_fixture(&client, &token, "memlist").await;

    let path = format!("/api/v4/channels/{channel_id}/members");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    teardown(&client, &token, &channel_id, &users).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );
    assert_eq!(
        rs_body.last(),
        Some(&b'\n'),
        "the encoder's newline ([D-086])"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    let list = parsed.as_array().expect("an array");
    assert_eq!(list.len(), 4, "the admin creator plus three plain members");
    for member in list {
        let own = member["user_id"].as_str() == Some(common::logged_in_user_id());
        if own {
            assert_ne!(
                member["last_viewed_at"].as_i64(),
                Some(-1),
                "the caller's own row keeps its timestamps"
            );
        } else {
            assert_eq!(
                member["last_viewed_at"].as_i64(),
                Some(-1),
                "everyone else's timestamps are blanked to -1"
            );
            assert_eq!(member["last_update_at"].as_i64(), Some(-1));
        }
    }
}

/// Pagination: two pages of two cover the four rows exactly, a page past the end is `[]`, and
/// every page is byte-identical across servers.
#[tokio::test]
async fn pages_split_cover_and_run_out_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (channel_id, users) = members_fixture(&client, &token, "mempage").await;

    let full = format!("/api/v4/channels/{channel_id}/members");
    let (_, full_body) = fetch_both_stable(&client, &token, &full).await;
    let all_ids = member_ids(&full_body);

    let mut paged_ids = Vec::new();
    for page in 0..2 {
        let path = format!("/api/v4/channels/{channel_id}/members?page={page}&per_page=2");
        let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "page {page} must agree byte for byte"
        );
        let ids = member_ids(&rs_body);
        assert_eq!(ids.len(), 2, "page {page} holds exactly two rows");
        paged_ids.extend(ids);
    }
    assert_eq!(
        paged_ids, all_ids,
        "two pages of two must cover the full list in the same order"
    );

    let past_the_end = format!("/api/v4/channels/{channel_id}/members?page=5&per_page=2");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &past_the_end).await;
    teardown(&client, &token, &channel_id, &users).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        "[]\n",
        "past the end is an empty array, not null — plus the encoder newline"
    );
}

/// The two parser traps: `per_page=0` serves **everything** (the store's `Limit > 0` guard, not
/// a zero-row page), and garbage pagination falls to the defaults rather than a 400.
#[tokio::test]
async fn per_page_zero_is_unlimited_and_garbage_falls_to_defaults() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (channel_id, users) = members_fixture(&client, &token, "memzero").await;

    let zero = format!("/api/v4/channels/{channel_id}/members?per_page=0");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &zero).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "per_page=0: the two servers must agree byte for byte"
    );
    assert_eq!(
        member_ids(&rs_body).len(),
        4,
        "per_page=0 is no limit at all — the whole channel comes back"
    );

    let garbage = format!("/api/v4/channels/{channel_id}/members?page=-3&per_page=nope");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &garbage).await;
    teardown(&client, &token, &channel_id, &users).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "garbage pagination: defaults, not a 400"
    );
    assert_eq!(
        member_ids(&rs_body).len(),
        4,
        "the defaults fit all four rows"
    );
}

/// The `read_channel` gate: a team member who never joined the channel is refused the roster —
/// and a missing channel is the same 403, because the gate's own lookup misses first.
#[tokio::test]
async fn non_members_and_missing_channels_are_403() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "memdenied").await;
    let outsider = create_plain_user(&client, &token, &team_id, "memdenied").await;

    let path = format!("/api/v4/channels/{channel_id}/members");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &outsider.token, &path).await;
    assert_eq!(go_status, 403, "no membership, no roster");
    assert_eq!(rs_status, 403);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "non-member");
    assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));

    let missing = "/api/v4/channels/zzzzzzzzzzzzzzzzzzzzzzzzzz/members";
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, missing).await;
    delete_channel(&client, &token, &channel_id).await;
    delete_plain_user(&client, &token, &outsider.id).await;
    assert_eq!(
        go_status, 403,
        "the gate's channel lookup misses and denies"
    );
    assert_eq!(rs_status, 403);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "missing channel");
}

/// Only GET is ours: POST on this path is Go's `addChannelMember`, reached through the forward —
/// and answered by Go's own body-validation 400 when given no body.
#[tokio::test]
async fn other_methods_on_this_path_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (channel_id, users) = members_fixture(&client, &token, "mempost").await;
    let path = format!("/api/v4/channels/{channel_id}/members");

    let response = client
        .post(format!("{RUST}{path}"))
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
        "POST is not migrated, so it must be forwarded"
    );
    let status = response.status().as_u16();
    let ours: serde_json::Value = response.json().await.expect("decodes");

    let direct = client
        .post(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(direct.status().as_u16(), status, "Go's own answer");
    let direct_body: serde_json::Value = direct.json().await.expect("decodes");
    assert_eq!(ours["id"], direct_body["id"]);

    teardown(&client, &token, &channel_id, &users).await;
}
