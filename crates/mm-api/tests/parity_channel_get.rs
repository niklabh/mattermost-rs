//! Cross-server parity for `GET /api/v4/channels/{channel_id}`.
//!
//! The first route that puts a whole `Channel` on the wire, the first with a *branching*
//! permission block (open channels ask the team, everything else asks the channel), and the
//! first whose body can carry a computed prop — `channel_mentions`, filled in from `~mentions`
//! in the header. So the suite leans on three shapes: byte-identical successes, refusals compared
//! field by field, and headers crafted so the prop has to be built rather than merely absent.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_channel_get
//! ```
//!
//! Every fixture is created and unwound here; [`common::purge_api_fixtures`] clears what a
//! panicking run leaves behind.

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_plain_user, delete_channel, delete_plain_user, fetch_both_raw, fetch_both_stable,
    go_minted_token, logged_in_user_id, post_message, purge_api_fixtures, stack_enabled,
};

/// Create a **private** channel on `team_id`. The shared helper hardcodes type `O`; the
/// private-channel cases are what this route's non-open permission branch needs.
async fn create_private_channel(
    client: &reqwest::Client,
    token: &str,
    team_id: &str,
    tag: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": "P",
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the private fixture channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Set a channel's header through Go's patch endpoint, so the header the two servers render
/// props from is one Go itself accepted and saved.
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

async fn own_team_and_channel(
    client: &reqwest::Client,
    token: &str,
    tag: &str,
) -> (String, String) {
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, token).await;
    let channel_id = create_channel(client, token, &team_id, tag).await;
    (team_id, channel_id)
}

/// The plain success: a member reads a channel they created, byte for byte, newline included.
///
/// The channel gets a root post **and a reply** first, so `total_msg_count` and
/// `total_msg_count_root` hold different numbers — two columns holding equal values cannot catch
/// a row-mapping that reads the wrong one, which is exactly the fixture lesson the unread suite
/// paid for last session.
#[tokio::test]
async fn the_channel_body_is_byte_identical_across_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel_id) = own_team_and_channel(&client, &token, "getbody").await;
    let root = post_message(&client, &token, &channel_id, "a root post", None).await;
    post_message(&client, &token, &channel_id, "a reply", Some(&root)).await;
    let path = format!("/api/v4/channels/{channel_id}");

    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_channel(&client, &token, &channel_id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );

    // Not vacuous: the body is the channel we made, and the encoder's newline is there ([D-086]).
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["id"].as_str(), Some(channel_id.as_str()));
    assert_eq!(parsed["type"].as_str(), Some("O"));
    assert_eq!(rs_body.last(), Some(&b'\n'));
    assert_ne!(
        parsed["total_msg_count"], parsed["total_msg_count_root"],
        "equal counters could not catch a swapped column"
    );
}

/// The `channel_mentions` prop, built rather than absent: the header names an open channel
/// (renders), a private channel (dropped — its display name must not leak), and a name that
/// matches nothing (dropped). `FillInChannelProps` on both sides, compared as bytes — which also
/// pins the key order of the prop map, Go's sorted-map marshalling against our `BTreeMap`.
#[tokio::test]
async fn a_header_mention_renders_the_open_channel_and_only_it() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, channel_id) = own_team_and_channel(&client, &token, "getmentions").await;
    let open_target = create_channel(&client, &token, &team_id, "gettargetopen").await;
    let private_target = create_private_channel(&client, &token, &team_id, "gettargetpriv").await;
    set_channel_header(
        &client,
        &token,
        &channel_id,
        "See ~mmrs-parity-gettargetopen, ~mmrs-parity-gettargetpriv and ~mmrs-parity-getnosuch",
    )
    .await;

    let path = format!("/api/v4/channels/{channel_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    for id in [&channel_id, &open_target, &private_target] {
        delete_channel(&client, &token, id).await;
    }

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(
        parsed["props"]["channel_mentions"],
        serde_json::json!({
            "mmrs-parity-gettargetopen": { "display_name": "mmrs parity gettargetopen" }
        }),
        "exactly the open channel renders — not the private one, not the missing one"
    );
}

/// An archived mention target stops rendering: `GetByNames` is the `DeleteAt = 0` variant. The
/// prop map comes back empty, and Go writes **no** prop for an empty map — `props` stays `null`.
#[tokio::test]
async fn an_archived_mention_target_does_not_render() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, channel_id) = own_team_and_channel(&client, &token, "getarchmain").await;
    let target = create_channel(&client, &token, &team_id, "getarchtarget").await;
    set_channel_header(
        &client,
        &token,
        &channel_id,
        "Gone: ~mmrs-parity-getarchtarget",
    )
    .await;
    delete_channel(&client, &token, &target).await; // archives, which is the point

    let path = format!("/api/v4/channels/{channel_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_channel(&client, &token, &channel_id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(
        parsed["props"],
        serde_json::Value::Null,
        "no resolvable mention, no prop — and the store never loads existing props here"
    );
}

/// The open-channel branch of the permission block: a **team** member who is not a channel
/// member reads a public channel through `read_public_channel` on the team. The channel gate
/// would deny this caller — no membership row — so a port that skipped the team gate 403s here.
#[tokio::test]
async fn a_team_member_reads_a_public_channel_without_joining_it() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, channel_id) = own_team_and_channel(&client, &token, "getpublic").await;
    let reader = create_plain_user(&client, &token, &team_id, "getreader").await;

    let path = format!("/api/v4/channels/{channel_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &reader.token, &path).await;
    delete_channel(&client, &token, &channel_id).await;
    delete_plain_user(&client, &token, &reader.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "a non-member team member must get the public channel from both servers"
    );
}

/// The non-open branch: the same non-member caller against a **private** channel is a 403 from
/// both servers — `read_public_channel` on the team must not open it, and with the
/// `DiscoverableChannels` flag off (its default) the discoverable surface never serves.
#[tokio::test]
async fn a_non_member_is_403_on_a_private_channel() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let private_id = create_private_channel(&client, &token, &team_id, "getprivdenied").await;
    let reader = create_plain_user(&client, &token, &team_id, "getprivreader").await;

    let path = format!("/api/v4/channels/{private_id}");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &reader.token, &path).await;
    delete_channel(&client, &token, &private_id).await;
    delete_plain_user(&client, &token, &reader.id).await;

    assert_eq!(go_status, 403);
    assert_eq!(rs_status, 403);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "private non-member");
    assert_eq!(
        go["id"].as_str(),
        Some("api.context.permissions.app_error"),
        "the ordinary permission refusal, not the discoverable surface"
    );
}

/// A well-formed id that matches nothing is a **404 from the fetch**, before any permission
/// check — this handler loads the channel first because the gate depends on its type. Contrast
/// `getChannelMember`, where a missing channel surfaces as the permission check's 403.
#[tokio::test]
async fn a_missing_channel_is_404_from_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/channels/zzzzzzzzzzzzzzzzzzzzzzzzzz";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;

    assert_eq!(go_status, 404);
    assert_eq!(rs_status, 404);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "missing channel");
    assert_eq!(
        go["id"].as_str(),
        Some("app.channel.get.existing.app_error")
    );
}

/// Archiving hides a channel from `GET …/unread` (its query filters `DeleteAt = 0`) but **not**
/// from this route — `SqlChannelStore.Get` has no such predicate. The 200 with a non-zero
/// `delete_at` is the other half of the asymmetry the unread suite pinned.
#[tokio::test]
async fn an_archived_channel_still_answers_200_here() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel_id) = own_team_and_channel(&client, &token, "getarchself").await;
    delete_channel(&client, &token, &channel_id).await; // archive it

    let path = format!("/api/v4/channels/{channel_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "both servers serve the archived channel"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_ne!(
        parsed["delete_at"].as_i64(),
        Some(0),
        "it really is archived"
    );
}

/// A DM: `TeamId == ""`, type `D` — the non-open branch again, granted by membership, and the
/// empty team id must reach the mention lookup as "every team" rather than as a predicate.
#[tokio::test]
async fn a_direct_channel_is_byte_identical() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let other = create_plain_user(&client, &token, &team_id, "getdmother").await;

    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!([logged_in_user_id(), other.id]))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the DM failed: {}",
        response.text().await.unwrap_or_default()
    );
    let dm: serde_json::Value = response.json().await.expect("the DM decodes");
    let dm_id = dm["id"].as_str().expect("an id").to_owned();

    let path = format!("/api/v4/channels/{dm_id}");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_plain_user(&client, &token, &other.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["type"].as_str(), Some("D"));
    assert_eq!(parsed["team_id"].as_str(), Some(""));
}

/// `?as_content_reviewer=true` takes Go's content-flagging path — Enterprise Advanced–licensed,
/// so this deployment answers 501 from `requireContentFlaggingAvailable` — and the whole request
/// is **forwarded**, so the answer is literally Go's. `=false` and garbage serve locally,
/// because Go's `strconv.ParseBool` error case is false.
#[tokio::test]
async fn the_content_reviewer_query_is_forwarded_and_only_when_true() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel_id) = own_team_and_channel(&client, &token, "getreviewer").await;

    // Forwarded: Go answers, and both answers carry Go's license error.
    for query in ["as_content_reviewer=true", "as_content_reviewer=1"] {
        let path = format!("/api/v4/channels/{channel_id}?{query}");
        let ours = client
            .get(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the Rust server answers");
        assert_eq!(
            ours.headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{query}: the content-reviewer path is not ours to answer"
        );
        let ours_status = ours.status().as_u16();
        let ours_body: serde_json::Value = ours.json().await.expect("decodes");

        let direct = client
            .get(format!("{GO}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(direct.status().as_u16(), ours_status, "{query}");
        let direct_body: serde_json::Value = direct.json().await.expect("decodes");
        assert_eq!(
            ours_body["id"], direct_body["id"],
            "{query}: same Go error either way"
        );
        assert_eq!(
            ours_status, 501,
            "{query}: unlicensed means not implemented"
        );
    }

    // Not forwarded: false and unparseable both take the normal path, served here.
    for query in ["as_content_reviewer=false", "as_content_reviewer=yes"] {
        let path = format!("/api/v4/channels/{channel_id}?{query}");
        let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&rs_body),
            String::from_utf8_lossy(&go_body),
            "{query}: ParseBool's error case is false, so this is the ordinary route"
        );
    }

    delete_channel(&client, &token, &channel_id).await;
}

/// [D-150] applies here too: a segment outside `[A-Za-z0-9]+` never matches Go's route, so the
/// mux 404 must come from Go rather than a 400 from our `IsValidId`.
#[tokio::test]
async fn a_non_alphanumeric_segment_answers_exactly_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = "/api/v4/channels/no-pe";

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
        "Go would not have routed this, so we must not handle it"
    );
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("body reads").to_vec();
    assert_eq!(status, 404);

    let direct = client
        .get(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(direct.status().as_u16(), status);
    assert_eq!(
        String::from_utf8_lossy(&direct.bytes().await.expect("body reads")),
        String::from_utf8_lossy(&body)
    );
}

/// `partially_migrated` again: only GET is ours, so a PUT (Go's `updateChannel`) must reach Go.
#[tokio::test]
async fn other_methods_on_this_path_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (_, channel_id) = own_team_and_channel(&client, &token, "getput").await;

    let response = client
        .put(format!("{RUST}/api/v4/channels/{channel_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "id": channel_id,
            "name": "mmrs-parity-getput",
            "display_name": "mmrs parity getput renamed",
            "type": "O",
        }))
        .send()
        .await
        .expect("the Rust server answers");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "PUT is not migrated, so it must be forwarded"
    );
    assert_eq!(
        response.status().as_u16(),
        200,
        "Go's updateChannel accepts it"
    );

    delete_channel(&client, &token, &channel_id).await;
}
