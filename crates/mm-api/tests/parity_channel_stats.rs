//! Cross-server parity for `GET /api/v4/channels/{channel_id}/stats`.
//!
//! The route has one permission gate and four counts, so the suite's centre of gravity is a
//! fixture whose four counts are **pairwise distinct** — the `getChannelUnread` lesson: two
//! fields holding the same number cannot catch a handler wiring a count to the wrong key.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_channel_stats
//! ```
//!
//! Guest accounts are config-gated off on Team Edition, so the guest count's non-zero case is
//! made by writing `SchemeGuest = TRUE` straight into the shared database **before either server
//! first reads the channel** — both servers then serve the same row, and Go has no stale cache
//! entry because nothing has populated one yet. The NULL-flag and deactivated-guest shapes stay
//! in `mm-store/tests/db_channel_stats.rs`, which owns the store-level predicates.

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_plain_user, delete_channel, delete_plain_user, fetch_both_raw, fetch_both_stable,
    go_minted_token, post_message, purge_api_fixtures, stack_enabled,
};

/// Pin a post — `POST /api/v4/posts/{post_id}/pin`. Go also drops a `channel_pinned_post`
/// system message into the channel, which is why the comparisons go through
/// [`common::fetch_both_stable`].
async fn pin_post(client: &reqwest::Client, token: &str, post_id: &str) {
    let response = client
        .post(format!("{GO}/api/v4/posts/{post_id}/pin"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "pinning {post_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Upload one file into a channel — the simple non-multipart form of `POST /api/v4/files` —
/// returning the new `FileInfo` id. The file is **not attached** to any post yet, so it does not
/// count until a post carries it (`PostId != ''`).
async fn upload_file(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    filename: &str,
) -> String {
    let response = client
        .post(format!(
            "{GO}/api/v4/files?channel_id={channel_id}&filename={filename}"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .body("mmrs parity file body")
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "uploading {filename} failed: {}",
        response.text().await.unwrap_or_default()
    );
    let uploaded: serde_json::Value = response.json().await.expect("the upload decodes");
    uploaded["file_infos"][0]["id"]
        .as_str()
        .expect("an id")
        .to_owned()
}

/// Post a message carrying `file_ids`, which is what stamps each file's `PostId`.
async fn post_with_files(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
    file_ids: &[String],
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "channel_id": channel_id,
            "message": "mmrs parity attachments",
            "file_ids": file_ids,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "posting with files failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the post decodes");
    created["id"].as_str().expect("an id").to_owned()
}

/// Write `SchemeGuest = TRUE` on one membership row, straight through the shared database —
/// the REST API cannot mint a guest on Team Edition. Returns `false` when `DATABASE_URL` is
/// unset so the caller can skip rather than fail.
async fn make_member_a_guest_in_db(channel_id: &str, user_id: &str) -> bool {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return false;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
    else {
        return false;
    };
    sqlx::query(
        "UPDATE channelmembers SET schemeguest = TRUE WHERE channelid = $1 AND userid = $2",
    )
    .bind(channel_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("the fixture member becomes a guest");
    true
}

/// Soft-delete a user through Go's API — `DELETE /users/{id}` sets `Users.DeleteAt` and leaves
/// the membership row in place, which is exactly the asymmetry the member count's join filters.
async fn deactivate_user(client: &reqwest::Client, admin_token: &str, user_id: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/users/{user_id}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "deactivating {user_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// The main dish: a channel where members=2, guests=1, pinned=3 and files=4 — pairwise distinct,
/// so every wrong wiring of a count to a key is a different number — byte-identical across both
/// servers, newline included.
#[tokio::test]
async fn the_stats_body_is_byte_identical_with_four_distinct_counts() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "statsmain").await;
    let member = create_plain_user(&client, &token, &team_id, "statsguest").await;
    common::add_user_to_channel(&client, &token, &channel_id, &member.id).await;

    // The guest flag goes in before either server first reads the channel — nothing has
    // populated Go's member-count caches yet, so there is no stale entry to diverge on.
    if !make_member_a_guest_in_db(&channel_id, &member.id).await {
        eprintln!("skipping: DATABASE_URL is needed to shape the guest row");
        return;
    }

    for tag in ["one", "two", "three"] {
        let post = post_message(&client, &token, &channel_id, &format!("pinned {tag}"), None).await;
        pin_post(&client, &token, &post).await;
    }

    let mut file_ids = Vec::new();
    for n in 1..=4 {
        file_ids.push(upload_file(&client, &token, &channel_id, &format!("f{n}.txt")).await);
    }
    post_with_files(&client, &token, &channel_id, &file_ids).await;

    let path = format!("/api/v4/channels/{channel_id}/stats");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_channel(&client, &token, &channel_id).await;
    delete_plain_user(&client, &token, &member.id).await;

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
    assert_eq!(parsed["channel_id"].as_str(), Some(channel_id.as_str()));
    let counts = [
        parsed["member_count"].as_i64().expect("a number"),
        parsed["guest_count"].as_i64().expect("a number"),
        parsed["pinnedpost_count"].as_i64().expect("a number"),
        parsed["files_count"].as_i64().expect("a number"),
    ];
    assert_eq!(counts, [2, 1, 3, 4], "the fixture's four distinct counts");
}

/// `?exclude_files_count=true` answers `-1` — the sentinel is the wire value — while an
/// unparseable value falls to `false` and counts, because Go discards `ParseBool`'s error.
#[tokio::test]
async fn excluding_the_files_count_serves_the_minus_one_sentinel() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "statsexcl").await;

    let excluded = format!("/api/v4/channels/{channel_id}/stats?exclude_files_count=true");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &excluded).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "excluded: the two servers must agree byte for byte"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(
        parsed["files_count"].as_i64(),
        Some(-1),
        "-1 is the wire value when excluded, not a missing key"
    );

    // `yes` is a ParseBool error, the error is discarded, and the count runs: 0 files here.
    let unparseable = format!("/api/v4/channels/{channel_id}/stats?exclude_files_count=yes");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &unparseable).await;
    delete_channel(&client, &token, &channel_id).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "unparseable: the two servers must agree byte for byte"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(
        parsed["files_count"].as_i64(),
        Some(0),
        "an unparseable flag counts — zero files, not the sentinel"
    );
}

/// Deactivating a member leaves the membership row and removes the user from the count — the
/// `Users.DeleteAt = 0` join, exercised through Go's own soft delete. The deactivation happens
/// **before** the channel's first stats read, so no cached count predates it on the Go side.
#[tokio::test]
async fn a_deactivated_member_stops_counting() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "statsdeact").await;
    let member = create_plain_user(&client, &token, &team_id, "statsdeact").await;
    common::add_user_to_channel(&client, &token, &channel_id, &member.id).await;
    deactivate_user(&client, &token, &member.id).await;

    let path = format!("/api/v4/channels/{channel_id}/stats");
    let (go_body, rs_body) = fetch_both_stable(&client, &token, &path).await;
    delete_channel(&client, &token, &channel_id).await;
    delete_plain_user(&client, &token, &member.id).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(
        parsed["member_count"].as_i64(),
        Some(1),
        "the creator counts; the deactivated member's surviving row does not"
    );
}

/// The gate is `read_channel` alone — no `read_public_channel` team fallback like `getChannel`'s
/// open branch — so a team member who never joined this **public** channel is refused its stats.
#[tokio::test]
async fn a_team_member_who_is_not_a_channel_member_is_403_even_on_a_public_channel() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "statsdenied").await;
    let outsider = create_plain_user(&client, &token, &team_id, "statsdenied").await;

    let path = format!("/api/v4/channels/{channel_id}/stats");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &outsider.token, &path).await;
    delete_channel(&client, &token, &channel_id).await;
    delete_plain_user(&client, &token, &outsider.id).await;

    assert_eq!(
        go_status, 403,
        "no team fallback on this route — Go refuses"
    );
    assert_eq!(rs_status, 403);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "stats non-member");
    assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));
}

/// A well-formed id that matches nothing is a **403, even for the system admin** — and it is a
/// 403, not a 404, because the handler never fetches the channel; the only lookup is the
/// permission gate's own, whose miss denies *before* the admin's roles are consulted
/// (authorization.go's channel fetch sits above every grant branch, including `manage_system`).
///
/// This test originally asserted the opposite — a 200 of zeroes, reasoned from "the admin branch
/// grants without a channel" — and both servers refused, in agreement. The zero-count answer the
/// store's `COUNT(*)` would give is unreachable over REST; it lives only in
/// `db_channel_stats.rs`.
#[tokio::test]
async fn a_missing_channel_is_a_403_even_for_the_admin() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/channels/zzzzzzzzzzzzzzzzzzzzzzzzzz/stats";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;

    assert_eq!(
        go_status, 403,
        "the gate's channel lookup misses and denies"
    );
    assert_eq!(rs_status, 403);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "stats missing");
    assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));
}

/// [D-150]: a segment outside `[A-Za-z0-9]+` never matches Go's route, so the mux 404 must come
/// from Go rather than a 400 from our `IsValidId`.
#[tokio::test]
async fn a_non_alphanumeric_segment_answers_exactly_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = "/api/v4/channels/no-pe/stats";

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

/// `partially_migrated` again: only GET is ours. Go registers no other method on this path, so a
/// POST is the gorilla-mux 405 — and it must be **Go's** 405, reached through the forward.
#[tokio::test]
async fn other_methods_on_this_path_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "statspost").await;
    let path = format!("/api/v4/channels/{channel_id}/stats");

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
    let body = response.bytes().await.expect("body reads").to_vec();

    let direct = client
        .post(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(direct.status().as_u16(), status, "Go's own method answer");
    assert_eq!(
        String::from_utf8_lossy(&direct.bytes().await.expect("body reads")),
        String::from_utf8_lossy(&body)
    );

    delete_channel(&client, &token, &channel_id).await;
}
