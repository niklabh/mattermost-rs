//! Cross-server parity for `channel_local.go`, `post_local.go` and `group_local.go` on the
//! socket — the 23 pairs `mm_api::local_channels` registers.
//!
//! Every write here lands in the **shared** database, so the pattern is one fixture per server:
//! a channel Go creates over its socket and a twin this server creates over its own, the same
//! operation applied to each, and the two *results* compared — the shape of the answer, and the
//! system post the operation leaves behind. The system posts are the point: every `local*`
//! handler has no user, and what Go does with that (post as the system bot, post as the added
//! user, post nothing) is the whole difference between these routes and their HTTP twins.
//!
//! Names carry the `mmrs-parity-` prefix, which `common::purge_api_fixtures` sweeps.

use super::super::common;
use super::super::common::local_socket::{
    assert_forwarded_body_is_gos, both, both_maybe_forwarded, go_socket, over_socket, rust_socket,
    sockets_enabled,
};

/// Serialises the tests that create or post as the system bot, because
/// [`the_system_bot_is_created_here_when_absent`] removes it — a concurrent `getOrCreateBot`
/// on the other side would then race the unique username.
static SYSTEM_BOT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|e| panic!("body is not JSON: {e}: {}", String::from_utf8_lossy(body)))
}

fn keys(value: &serde_json::Value) -> Vec<&str> {
    value
        .as_object()
        .map(|o| o.keys().map(String::as_str).collect())
        .unwrap_or_default()
}

/// One request with a body over one socket.
async fn over_socket_with_body(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: String,
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("Host", "localhost")
        .header("Content-Type", "application/json")
        .header("Content-Length", body.len().to_string())
        .body(axum::body::Body::from(body))
        .expect("request builds");
    let response = mm_api::local::send_over_unix(socket, request)
        .await
        .unwrap_or_else(|e| panic!("{method} {path} over {}: {e}", socket.display()));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, headers, body)
}

/// The same operation on Go's fixture over Go's socket and on ours over ours, with the
/// "we served it" check on our side.
async fn each_side(
    method: &str,
    go_path: &str,
    rust_path: &str,
    body: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let go = go_socket().expect("checked by sockets_enabled");
    let rust = rust_socket().expect("checked by sockets_enabled");
    let (go_status, _, go_body) =
        over_socket_with_body(&go, method, go_path, body.to_owned()).await;
    let (rs_status, rs_headers, rs_body) =
        over_socket_with_body(&rust, method, rust_path, body.to_owned()).await;
    assert_eq!(
        rs_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "{method} {rust_path} was forwarded to Go over the socket"
    );
    ((go_status, go_body), (rs_status, rs_body))
}

/// [`both`] for a list that other suites may be writing to concurrently: retried until the
/// two answers agree, so a churning list cannot fail a test about the route.
async fn both_stable(method: &str, path: &str) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let mut last = both(method, path).await;
    for _ in 0..30 {
        if last.0 == last.1 {
            return last;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        last = both(method, path).await;
    }
    last
}

/// The newest post in a channel, as either server reports it over its socket.
async fn latest_post(socket: &std::path::Path, channel_id: &str) -> serde_json::Value {
    let (status, _, body) = over_socket(
        socket,
        "GET",
        &format!("/api/v4/channels/{channel_id}/posts?per_page=1"),
    )
    .await;
    assert_eq!(status, 200, "posts of {channel_id}");
    let list = json(&body);
    let id = list["order"][0]
        .as_str()
        .unwrap_or_else(|| panic!("{channel_id} has no posts: {list}"));
    list["posts"][id].clone()
}

/// Wait for a channel's newest post to be of `post_type` — Go posts the unarchive notice from a
/// goroutine, after the response.
async fn latest_post_of_type(
    socket: &std::path::Path,
    channel_id: &str,
    post_type: &str,
) -> serde_json::Value {
    for _ in 0..40 {
        let post = latest_post(socket, channel_id).await;
        if post["type"] == post_type {
            return post;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no {post_type} post appeared in {channel_id}");
}

/// Two system posts, one per server's fixture, are the same notice: type, author, message and
/// props — everything but the ids and timestamps the write mints.
fn assert_same_notice(go_post: &serde_json::Value, rs_post: &serde_json::Value, what: &str) {
    for field in ["type", "user_id", "message", "props"] {
        assert_eq!(go_post[field], rs_post[field], "{what}: {field}");
    }
}

async fn post_count(socket: &std::path::Path, channel_id: &str) -> usize {
    let (status, _, body) = over_socket(
        socket,
        "GET",
        &format!("/api/v4/channels/{channel_id}/posts"),
    )
    .await;
    assert_eq!(status, 200);
    json(&body)["order"].as_array().map_or(0, Vec::len)
}

/// The **reads**, all eleven shared handlers plus the two group lists, against one fixture.
///
/// The fixture is Go-made over HTTP so the admin is a member; the reads then go through both
/// sockets with no credential at all, and every gate on their path passes on `Session.Local`.
#[tokio::test]
async fn the_channel_reads_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let admin = common::logged_in_user_id();
    let tag = format!("locrd{}", &mm_model::utils::new_id()[..6]);
    let team = common::create_team(&client, &token, &tag).await;
    let public = common::create_channel(&client, &token, &team, &tag).await;
    let private =
        common::create_channel_typed(&client, &token, &team, &format!("{tag}-p"), "P").await;
    let archived = common::create_channel(&client, &token, &team, &format!("{tag}-a")).await;
    common::delete_channel(&client, &token, &archived).await;
    let post = common::post_message(&client, &token, &public, "over the socket", None).await;
    common::invalidate_go_caches(&client, &token).await;

    for path in [
        format!("/api/v4/channels/{public}"),
        format!("/api/v4/channels/{private}"),
        format!("/api/v4/teams/{team}/channels"),
        format!("/api/v4/teams/{team}/channels/private"),
        format!("/api/v4/teams/{team}/channels/deleted"),
        format!("/api/v4/teams/{team}/channels/name/mmrs-parity-{tag}"),
        format!("/api/v4/teams/{team}/channels/name/mmrs-parity-{tag}-a?include_deleted=true"),
        format!("/api/v4/teams/name/mmrs-parity-{tag}/channels/name/mmrs-parity-{tag}-p"),
        format!("/api/v4/channels/{public}/members"),
        format!("/api/v4/channels/{public}/members/{admin}"),
        format!("/api/v4/channels/{public}/posts"),
        format!("/api/v4/posts/{post}"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!(go_status, 200, "{path}: Go's status");
        assert_eq!(rs_status, go_status, "{path}: our status");
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body),
            "{path}"
        );
    }

    // `getAllChannels` reads every channel on the installation, which other suites are creating
    // and deleting under us; the comparison is retried until the two agree.
    for path in [
        "/api/v4/channels?per_page=200",
        "/api/v4/channels?per_page=5&include_total_count=true",
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both_stable("GET", path).await;
        assert_eq!((go_status, rs_status), (200, 200), "{path}");
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body),
            "{path}"
        );
    }

    // The group lists: no `requireLicense` on the local handlers, so an unlicensed installation
    // is `getGroupsBy*Common`'s own 403 rather than the HTTP twin's 501.
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    for path in [
        format!("/api/v4/channels/{public}/groups"),
        format!("/api/v4/teams/{team}/groups"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!(go_status, 403, "{path}: Go's status");
        assert_eq!(rs_status, go_status, "{path}: our status");
        let go = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(go["id"], "api.ldap_groups.license_error");
    }

    // `me` is the empty user id on this transport.
    let path = format!("/api/v4/channels/{public}/members/me");
    let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
    assert_eq!(go_status, 400, "{path}: Go's status");
    assert_eq!(rs_status, go_status);
    let go = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}

/// The **writes**, each applied to a Go-made twin over Go's socket and to our twin over ours.
///
/// The channels are created over the sockets too — `localCreateChannel` has no creator and adds
/// no member, so both start with an empty member list and an empty `creator_id`. Every system
/// post the writes leave behind is compared between the twins: the same type, the same author
/// (the system bot for archive, restore, privacy and removal; the added user for the join; nobody
/// for the move), the same message and props.
#[tokio::test]
async fn the_local_channel_writes_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let _bot = SYSTEM_BOT.lock().await;
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let tag = format!("locwr{}", &mm_model::utils::new_id()[..6]);
    let team = common::create_team(&client, &token, &tag).await;
    let other_team = common::create_team(&client, &token, &format!("{tag}-2")).await;
    let plain = common::create_plain_user(&client, &token, &team, &tag).await;
    // The plain user has to be on the destination team for the forced move to keep them.
    let response = client
        .post(format!("{}/api/v4/teams/{other_team}/members", common::GO))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"team_id": other_team, "user_id": plain.id}))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "adding to the second team");
    common::invalidate_go_caches(&client, &token).await;
    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");

    // ---- localCreateChannel
    let body_for = |suffix: &str| {
        serde_json::json!({
            "team_id": team,
            "name": format!("mmrs-parity-{tag}-{suffix}"),
            "display_name": format!("mmrs parity {tag} {suffix}"),
            "type": "O",
            "header": "made over the socket",
        })
        .to_string()
    };
    let (go_status, _, go_body) =
        over_socket_with_body(&go, "POST", "/api/v4/channels", body_for("go")).await;
    let (rs_status, rs_headers, rs_body) =
        over_socket_with_body(&rust, "POST", "/api/v4/channels", body_for("rs")).await;
    assert_eq!(
        rs_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "the create was forwarded"
    );
    assert_eq!(
        go_status,
        201,
        "Go creates over its socket: {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    let (go_channel, rs_channel) = (json(&go_body), json(&rs_body));
    assert_eq!(
        keys(&go_channel),
        keys(&rs_channel),
        "the created channel's fields"
    );
    assert_eq!(go_channel["creator_id"], "", "no session, no creator");
    assert_eq!(rs_channel["creator_id"], "");
    assert!(
        go_body.ends_with(b"\n") && rs_body.ends_with(b"\n"),
        "Encode's newline"
    );
    let theirs = go_channel["id"].as_str().expect("an id").to_owned();
    let mine = rs_channel["id"].as_str().expect("an id").to_owned();

    // Each twin, read by both servers: byte for byte, and with no members at all.
    for id in [&theirs, &mine] {
        for path in [
            format!("/api/v4/channels/{id}"),
            format!("/api/v4/channels/{id}/members"),
        ] {
            let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
            assert_eq!((go_status, rs_status), (200, 200), "{path}");
            assert_eq!(go_body, rs_body, "{path}");
        }
        let (_, _, members) =
            over_socket(&go, "GET", &format!("/api/v4/channels/{id}/members")).await;
        assert_eq!(json(&members), serde_json::json!([]), "addMember is false");
    }

    // ---- localPatchChannel: no permission, no "no changes" check.
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "PUT",
        &format!("/api/v4/channels/{theirs}/patch"),
        &format!("/api/v4/channels/{mine}/patch"),
        r#"{"header":"patched over the socket","purpose":"a purpose"}"#,
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    let (go_patched, rs_patched) = (json(&go_body), json(&rs_body));
    assert_eq!(keys(&go_patched), keys(&rs_patched));
    assert_eq!(rs_patched["header"], "patched over the socket");
    assert_eq!(rs_patched["purpose"], go_patched["purpose"]);
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "PUT",
        &format!("/api/v4/channels/{theirs}/patch"),
        &format!("/api/v4/channels/{mine}/patch"),
        "{}",
    )
    .await;
    assert_eq!(
        (go_status, rs_status),
        (200, 200),
        "an empty patch is a 200 here"
    );
    assert_eq!(keys(&json(&go_body)), keys(&json(&rs_body)));
    // A blank display name is **accepted** — measured: `Channel.IsValid` has no minimum for
    // it — so this is a 200 on both, and the row now carries the empty name.
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "PUT",
        &format!("/api/v4/channels/{theirs}/patch"),
        &format!("/api/v4/channels/{mine}/patch"),
        r#"{"display_name":""}"#,
    )
    .await;
    assert_eq!(
        (go_status, rs_status),
        (200, 200),
        "a blank display name is accepted"
    );
    assert_eq!(json(&go_body)["display_name"], "");
    assert_eq!(json(&rs_body)["display_name"], "");

    // ---- localAddChannelMember: no requestor, so the notice is the *joined* user's.
    let add = serde_json::json!({"user_id": plain.id}).to_string();
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "POST",
        &format!("/api/v4/channels/{theirs}/members"),
        &format!("/api/v4/channels/{mine}/members"),
        &add,
    )
    .await;
    assert_eq!(go_status, 201, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    let (go_member, rs_member) = (json(&go_body), json(&rs_body));
    assert_eq!(keys(&go_member), keys(&rs_member), "the member's fields");
    assert_eq!(rs_member["user_id"], plain.id);
    assert_eq!(rs_member["channel_id"], mine);
    let go_socket_ = go_socket().expect("checked");
    let rust_socket_ = rust_socket().expect("checked");
    let (go_join, rs_join) = (
        latest_post(&go_socket_, &theirs).await,
        latest_post(&rust_socket_, &mine).await,
    );
    assert_same_notice(&go_join, &rs_join, "the join notice");
    assert_eq!(go_join["type"], "system_join_channel");
    assert_eq!(
        go_join["user_id"], plain.id,
        "posted by the user who joined"
    );
    // Adding the same user again answers the existing row, still 201.
    let ((go_status, _), (rs_status, rs_body)) = each_side(
        "POST",
        &format!("/api/v4/channels/{theirs}/members"),
        &format!("/api/v4/channels/{mine}/members"),
        &add,
    )
    .await;
    assert_eq!((go_status, rs_status), (201, 201), "an existing member");
    assert_eq!(json(&rs_body)["user_id"], plain.id);
    // A post_root_id from the channel is accepted and changes nothing.
    let rooted = serde_json::json!({
        "user_id": plain.id,
        "post_root_id": rs_join["id"],
    })
    .to_string();
    let (rs_status, rs_headers, _) = over_socket_with_body(
        &rust,
        "POST",
        &format!("/api/v4/channels/{mine}/members"),
        rooted,
    )
    .await;
    assert_eq!(rs_status, 201);
    assert_eq!(
        rs_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "a valid post_root_id is served here"
    );

    // ---- localRemoveChannelMember: the notice is the system bot's.
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "DELETE",
        &format!("/api/v4/channels/{theirs}/members/{}", plain.id),
        &format!("/api/v4/channels/{mine}/members/{}", plain.id),
        "",
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(rs_body, br#"{"status":"OK"}"#);
    assert_eq!(go_body, rs_body);
    let (go_removed, rs_removed) = (
        latest_post(&go_socket_, &theirs).await,
        latest_post(&rust_socket_, &mine).await,
    );
    assert_same_notice(&go_removed, &rs_removed, "the removal notice");
    assert_eq!(go_removed["type"], "system_remove_from_channel");
    let system_bot = go_removed["user_id"]
        .as_str()
        .expect("an author")
        .to_owned();
    assert_ne!(system_bot, "", "posted by the system bot, not by nobody");
    assert_eq!(
        rs_removed["user_id"], system_bot,
        "the same bot on both sides"
    );

    // ---- localUpdateChannelPrivacy: no convert permission; the notice is the system bot's.
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "PUT",
        &format!("/api/v4/channels/{theirs}/privacy"),
        &format!("/api/v4/channels/{mine}/privacy"),
        r#"{"privacy":"P"}"#,
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(keys(&json(&go_body)), keys(&json(&rs_body)));
    assert_eq!(json(&rs_body)["type"], "P");
    let (go_privacy, rs_privacy) = (
        latest_post(&go_socket_, &theirs).await,
        latest_post(&rust_socket_, &mine).await,
    );
    assert_same_notice(&go_privacy, &rs_privacy, "the privacy notice");
    assert_eq!(go_privacy["type"], "system_change_chan_privacy");
    assert_eq!(go_privacy["user_id"], system_bot);

    // ---- localMoveChannel: `user == nil`, so no "moved" notice at all.
    let before = (
        post_count(&go_socket_, &theirs).await,
        post_count(&rust_socket_, &mine).await,
    );
    let mv = serde_json::json!({"team_id": other_team, "force": true}).to_string();
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "POST",
        &format!("/api/v4/channels/{theirs}/move"),
        &format!("/api/v4/channels/{mine}/move"),
        &mv,
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(keys(&json(&go_body)), keys(&json(&rs_body)));
    assert_eq!(json(&rs_body)["team_id"], other_team);
    assert_eq!(json(&go_body)["team_id"], other_team);
    let after = (
        post_count(&go_socket_, &theirs).await,
        post_count(&rust_socket_, &mine).await,
    );
    assert_eq!(after, before, "a local move posts nothing");
    let path = format!("/api/v4/teams/{other_team}/channels/private");
    let ((_, go_list), (_, rs_list)) = both("GET", &path).await;
    assert_eq!(go_list, rs_list, "{path}: both twins moved");
    assert!(String::from_utf8_lossy(&rs_list).contains(&mine));

    // ---- localDeletePost: a soft delete of the join notice, then a permanent one of the
    // privacy notice; neither needs a permission and `DeleteBy` is nobody.
    let (go_join_id, rs_join_id) = (
        go_join["id"].as_str().expect("id"),
        rs_join["id"].as_str().expect("id"),
    );
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "DELETE",
        &format!("/api/v4/posts/{go_join_id}"),
        &format!("/api/v4/posts/{rs_join_id}"),
        "",
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(go_body, rs_body);
    for id in [go_join_id, rs_join_id] {
        let path = format!("/api/v4/posts/{id}");
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!(go_status, 404, "{path}: soft-deleted");
        assert_eq!(rs_status, go_status);
        common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        // `?include_deleted` needs manage_system, which the local session has.
        let path = format!("/api/v4/posts/{id}?include_deleted=true");
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!((go_status, rs_status), (200, 200), "{path}");
        assert_eq!(go_body, rs_body, "{path}");
    }
    let (go_privacy_id, rs_privacy_id) = (
        go_privacy["id"].as_str().expect("id"),
        rs_privacy["id"].as_str().expect("id"),
    );
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "DELETE",
        &format!("/api/v4/posts/{go_privacy_id}?permanent=true"),
        &format!("/api/v4/posts/{rs_privacy_id}?permanent=true"),
        "",
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    for id in [go_privacy_id, rs_privacy_id] {
        let path = format!("/api/v4/posts/{id}?include_deleted=true");
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!(go_status, 404, "{path}: gone for good");
        assert_eq!(rs_status, go_status);
        common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    }
    // A second permanent delete of the same id is the 404 from the fetch, on both.
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "DELETE",
        &format!("/api/v4/posts/{go_privacy_id}?permanent=true"),
        &format!("/api/v4/posts/{rs_privacy_id}?permanent=true"),
        "",
    )
    .await;
    assert_eq!((go_status, rs_status), (404, 404));
    let go = common::assert_error_bodies_match_except_known_gaps(
        &go_body,
        &rs_body,
        "permanent delete of a missing post",
    );
    assert_eq!(go["id"], "app.post.get.app_error");

    // ---- localDeleteChannel (archive): the notice is the system bot's.
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "DELETE",
        &format!("/api/v4/channels/{theirs}"),
        &format!("/api/v4/channels/{mine}"),
        "",
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(go_body, rs_body);
    let (go_archived, rs_archived) = (
        latest_post(&go_socket_, &theirs).await,
        latest_post(&rust_socket_, &mine).await,
    );
    assert_same_notice(&go_archived, &rs_archived, "the archive notice");
    assert_eq!(go_archived["type"], "system_channel_deleted");
    assert_eq!(go_archived["user_id"], system_bot);
    // Go caches channels by id and nothing we wrote over *our* socket invalidated its copy of
    // our twin — it still held the row as created. The reads below are of the row, so drop the
    // cache first; a stale Go read here would be a finding about the stack, not the route.
    common::invalidate_go_caches(&client, &token).await;
    for id in [&theirs, &mine] {
        let path = format!("/api/v4/channels/{id}");
        let ((_, go_body), (_, rs_body)) = both("GET", &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body),
            "{path}"
        );
        assert_ne!(json(&rs_body)["delete_at"], 0, "archived");
    }
    // Archiving again is the app layer's 400, on both.
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "DELETE",
        &format!("/api/v4/channels/{theirs}"),
        &format!("/api/v4/channels/{mine}"),
        "",
    )
    .await;
    assert_eq!((go_status, rs_status), (400, 400));
    let go = common::assert_error_bodies_match_except_known_gaps(
        &go_body,
        &rs_body,
        "archiving an archived channel",
    );
    assert_eq!(go["id"], "api.channel.delete_channel.deleted.app_error");

    // ---- localRestoreChannel: the notice is the system bot's, from a goroutine on Go's side.
    let ((go_status, go_body), (rs_status, rs_body)) = each_side(
        "POST",
        &format!("/api/v4/channels/{theirs}/restore"),
        &format!("/api/v4/channels/{mine}/restore"),
        "",
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        rs_status,
        go_status,
        "{}",
        String::from_utf8_lossy(&rs_body)
    );
    assert_eq!(keys(&json(&go_body)), keys(&json(&rs_body)));
    assert_eq!(json(&rs_body)["delete_at"], 0);
    let (go_restored, rs_restored) = (
        latest_post_of_type(&go_socket_, &theirs, "system_channel_restored").await,
        latest_post_of_type(&rust_socket_, &mine, "system_channel_restored").await,
    );
    assert_same_notice(&go_restored, &rs_restored, "the restore notice");
    assert_eq!(go_restored["user_id"], system_bot);

    // ---- DELETE ?permanent=true: forwarded here (D-610). Ours over our socket alone — sent to
    // Go's socket first it would be gone before we looked, and the 404 we then answer ourselves
    // (the channel lookup precedes the forward) would say nothing about the forward.
    let (rs_status, rs_headers, rs_body) = over_socket(
        &rust_socket_,
        "DELETE",
        &format!("/api/v4/channels/{mine}?permanent=true"),
    )
    .await;
    assert_ne!(
        rs_headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust"),
        "the permanent channel delete is forwarded"
    );
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    let (go_status, _, go_body) = over_socket(
        &go_socket_,
        "DELETE",
        &format!("/api/v4/channels/{theirs}?permanent=true"),
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(
        go_body, rs_body,
        "Go's status body, through the forward and directly"
    );
    common::invalidate_go_caches(&client, &token).await;
    for id in [&theirs, &mine] {
        let path = format!("/api/v4/channels/{id}");
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!((go_status, rs_status), (404, 404), "{path}: gone");
        common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    }

    common::delete_plain_user(&client, &token, &plain.id).await;
}

/// The refusals of the eight `local*` channel handlers and `localDeletePost`, over both sockets
/// against the same fixture — none of these writes anything.
#[tokio::test]
async fn the_local_write_refusals_match() {
    if !sockets_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let admin = common::logged_in_user_id();
    let tag = format!("locrf{}", &mm_model::utils::new_id()[..6]);
    let team = common::create_team(&client, &token, &tag).await;
    let channel = common::create_channel(&client, &token, &team, &tag).await;
    let other = common::create_channel(&client, &token, &team, &format!("{tag}-o")).await;
    let plain = common::create_plain_user(&client, &token, &team, &tag).await;
    let dm = common::create_direct_channel(&client, &token, admin, &plain.id).await;
    let elsewhere = common::post_message(&client, &token, &other, "another channel", None).await;
    let unknown = "zzzzzzzzzzzzzzzzzzzzzzzzzz";
    common::invalidate_go_caches(&client, &token).await;

    let default_channel = {
        let path = format!("/api/v4/teams/{team}/channels/name/town-square");
        let ((status, body), _) = both("GET", &path).await;
        assert_eq!(status, 200, "{path}");
        json(&body)["id"].as_str().expect("an id").to_owned()
    };

    let cases: Vec<(&str, String, String, u16, &str)> = vec![
        // localCreateChannel: the decode, then the store's own validation — no handler 400s.
        // (An empty `team_id` is **not** a refusal here: the model does not require one and
        // both servers answer 201 with `team_id: ""` — measured — so the case that refuses is
        // an invalid type, which writes nothing.)
        (
            "POST",
            "/api/v4/channels".into(),
            "null".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "POST",
            "/api/v4/channels".into(),
            "not json".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "POST",
            "/api/v4/channels".into(),
            format!(
                r#"{{"team_id":"{team}","name":"mmrs-parity-{tag}-z","display_name":"x","type":"Z"}}"#
            ),
            400,
            "model.channel.is_valid.type.app_error",
        ),
        // localDeleteChannel
        (
            "DELETE",
            format!("/api/v4/channels/{unknown}"),
            "".to_owned(),
            404,
            "app.channel.get.existing.app_error",
        ),
        (
            "DELETE",
            format!("/api/v4/channels/{dm}"),
            "".to_owned(),
            400,
            "api.channel.delete_channel.type.invalid",
        ),
        (
            "DELETE",
            format!("/api/v4/channels/{dm}?permanent=true"),
            "".to_owned(),
            400,
            "api.channel.delete_channel.type.invalid",
        ),
        (
            "DELETE",
            format!("/api/v4/channels/{default_channel}"),
            "".to_owned(),
            400,
            "api.channel.delete_channel.cannot.app_error",
        ),
        // localPatchChannel
        (
            "PUT",
            format!("/api/v4/channels/{channel}/patch"),
            "null".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "PUT",
            format!("/api/v4/channels/{unknown}/patch"),
            r#"{"header":"h"}"#.to_owned(),
            404,
            "app.channel.get.existing.app_error",
        ),
        // localMoveChannel: channel, then the two body fields, then the team, then the type.
        (
            "POST",
            format!("/api/v4/channels/{unknown}/move"),
            "{}".to_owned(),
            404,
            "app.channel.get.existing.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/move"),
            r#"{"team_id":1}"#.to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/move"),
            r#"{"team_id":"x","force":"true"}"#.to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/move"),
            format!(r#"{{"team_id":"{unknown}","force":false}}"#),
            404,
            "app.team.get.find.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{dm}/move"),
            format!(r#"{{"team_id":"{team}","force":false}}"#),
            403,
            "api.channel.move_channel.type.invalid",
        ),
        // localUpdateChannelPrivacy
        (
            "PUT",
            format!("/api/v4/channels/{channel}/privacy"),
            r#"{"privacy":"X"}"#.to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "PUT",
            format!("/api/v4/channels/{unknown}/privacy"),
            r#"{"privacy":"P"}"#.to_owned(),
            404,
            "app.channel.get.existing.app_error",
        ),
        (
            "PUT",
            format!("/api/v4/channels/{default_channel}/privacy"),
            r#"{"privacy":"P"}"#.to_owned(),
            400,
            "api.channel.update_channel_privacy.default_channel_error",
        ),
        // localRestoreChannel
        (
            "POST",
            format!("/api/v4/channels/{unknown}/restore"),
            "".to_owned(),
            404,
            "app.channel.get.existing.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/restore"),
            "".to_owned(),
            400,
            "api.channel.restore_channel.restored.app_error",
        ),
        // localRemoveChannelMember
        (
            "DELETE",
            format!("/api/v4/channels/{channel}/members/{unknown}"),
            "".to_owned(),
            404,
            "app.user.missing_account.const",
        ),
        (
            "DELETE",
            format!("/api/v4/channels/{dm}/members/{admin}"),
            "".to_owned(),
            400,
            "api.channel.remove_channel_member.type.app_error",
        ),
        (
            "DELETE",
            format!("/api/v4/channels/{channel}/members/me"),
            "".to_owned(),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "DELETE",
            format!("/api/v4/channels/{channel}/members/{}", plain.id),
            "".to_owned(),
            404,
            "app.channel.get_member.missing.app_error",
        ),
        // localAddChannelMember: `user_id` is named alone, unlike the HTTP handler.
        (
            "POST",
            format!("/api/v4/channels/{channel}/members"),
            r#"{"user_ids":["x"]}"#.to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/members"),
            format!(r#"{{"user_id":"{admin}","post_root_id":"bad"}}"#),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/members"),
            format!(r#"{{"user_id":"{admin}","post_root_id":"{unknown}"}}"#),
            404,
            "app.post.get.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/members"),
            format!(r#"{{"user_id":"{admin}","post_root_id":"{elsewhere}"}}"#),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{unknown}/members"),
            format!(r#"{{"user_id":"{admin}"}}"#),
            404,
            "app.channel.get.existing.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{dm}/members"),
            format!(r#"{{"user_id":"{admin}"}}"#),
            400,
            "api.channel.add_user_to_channel.type.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/members"),
            format!(r#"{{"user_id":"{unknown}"}}"#),
            404,
            "app.user.missing_account.const",
        ),
        // localDeletePost
        (
            "DELETE",
            format!("/api/v4/posts/{unknown}"),
            "".to_owned(),
            404,
            "app.post.get.app_error",
        ),
        (
            "DELETE",
            format!("/api/v4/posts/{unknown}?permanent=true"),
            "".to_owned(),
            404,
            "app.post.get.app_error",
        ),
        // The group lists: the id before the licence.
        (
            "GET",
            "/api/v4/channels/notanid/groups".into(),
            "".to_owned(),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "GET",
            "/api/v4/teams/notanid/groups".into(),
            "".to_owned(),
            400,
            "api.context.invalid_url_param.app_error",
        ),
    ];

    let go = go_socket().expect("checked");
    let rust = rust_socket().expect("checked");
    for (method, path, body, expected, id) in &cases {
        let (go_status, _, go_body) = over_socket_with_body(&go, method, path, body.clone()).await;
        let (rs_status, rs_headers, rs_body) =
            over_socket_with_body(&rust, method, path, body.clone()).await;
        assert_eq!(
            rs_headers
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust"),
            "{method} {path} was forwarded"
        );
        assert_eq!(
            go_status,
            *expected,
            "{method} {path} {body}: Go's status: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            go_status,
            "{method} {path} {body}: our status: {}",
            String::from_utf8_lossy(&rs_body)
        );
        let context = format!("{method} {path} {body}");
        let go = common::assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
        assert_eq!(go["id"], *id, "{context}");
    }

    common::delete_plain_user(&client, &token, &plain.id).await;
}

/// The routes beside these registrations still answer, and the pairs this family does **not**
/// register still reach Go over the socket — including the segments outside the mux class.
#[tokio::test]
async fn the_neighbouring_local_routes_still_answer_and_the_rest_forward() {
    if !sockets_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let admin = common::logged_in_user_id();
    let tag = format!("locnb{}", &mm_model::utils::new_id()[..6]);
    let team = common::create_team(&client, &token, &tag).await;
    let channel = common::create_channel(&client, &token, &team, &tag).await;
    common::invalidate_go_caches(&client, &token).await;

    // Served neighbours from the other families, untouched by the merge.
    for path in [
        "/api/v4/system/ping".to_owned(),
        format!("/api/v4/users/{admin}/status"),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) = both("GET", &path).await;
        assert_eq!((go_status, rs_status), (200, 200), "{path}");
        assert_eq!(go_body, rs_body, "{path}");
    }

    // Unregistered pairs on registered paths, and paths this family leaves alone: forwarded,
    // and Go's local mux answers its own 404 for a route it does not have either.
    for (method, path) in [
        ("PUT", format!("/api/v4/channels/{channel}")),
        // `getChannel`'s reviewer branch, decided before the handler so it forwards over the
        // socket rather than the handler's port.
        (
            "GET",
            format!("/api/v4/channels/{channel}?as_content_reviewer=true"),
        ),
        ("GET", format!("/api/v4/channels/{channel}/stats")),
        ("POST", format!("/api/v4/teams/{team}/channels/search")),
        (
            "PUT",
            format!("/api/v4/posts/{}", "zzzzzzzzzzzzzzzzzzzzzzzzzz"),
        ),
        // Outside the mux class: `{channel_id:[A-Za-z0-9]+}`, `{channel_name:[A-Za-z0-9_-]+}`,
        // `{team_name:[A-Za-z0-9_-]+}`.
        ("GET", "/api/v4/channels/bad-id".to_owned()),
        (
            "GET",
            format!("/api/v4/teams/{team}/channels/name/bad.name"),
        ),
        (
            "GET",
            "/api/v4/teams/name/bad.team/channels/name/x".to_owned(),
        ),
        ("GET", "/api/v4/posts/bad-id".to_owned()),
    ] {
        let ((go_status, go_body), (rs_status, rs_body), served_here) =
            both_maybe_forwarded(method, &path).await;
        assert!(!served_here, "{method} {path} must be forwarded");
        assert_eq!(rs_status, go_status, "{method} {path}");
        assert_forwarded_body_is_gos(&go_body, &rs_body, &path);
    }
}

/// **The system bot is created by this server when it is absent**, with Go's shape: a
/// `system-bot` user with the `system_user` role, a `Bots` row named "System" and owned by the
/// first administrator by username — and Go then reuses it rather than making its own.
///
/// Removes the bot first, which is why this and every test that posts as the bot share
/// [`SYSTEM_BOT`].
#[tokio::test]
async fn the_system_bot_is_created_here_when_absent() {
    if !sockets_enabled() {
        return;
    }
    let _bot = SYSTEM_BOT.lock().await;
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    let client = common::client();
    let token = common::go_minted_token(&client).await;
    let tag = format!("locsb{}", &mm_model::utils::new_id()[..6]);
    let team = common::create_team(&client, &token, &tag).await;
    let mine = common::create_channel(&client, &token, &team, &format!("{tag}-rs")).await;
    let theirs = common::create_channel(&client, &token, &team, &format!("{tag}-go")).await;

    // Remove the bot: every row keyed on its user, then the user.
    let existing: Vec<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = 'system-bot'")
            .fetch_all(&pool)
            .await
            .expect("the lookup runs");
    for (id,) in &existing {
        for table in [
            "bots WHERE userid = $1",
            "channelmembers WHERE userid = $1",
            "teammembers WHERE userid = $1",
            "sessions WHERE userid = $1",
            "preferences WHERE userid = $1",
            "status WHERE userid = $1",
            "users WHERE id = $1",
        ] {
            sqlx::query(&format!("DELETE FROM {table}"))
                .bind(id)
                .execute(&pool)
                .await
                .expect("the delete runs");
        }
    }
    common::invalidate_go_caches(&client, &token).await;

    // Ours first: the archive posts as a bot that does not exist yet.
    let rust = rust_socket().expect("checked");
    let (status, headers, body) =
        over_socket(&rust, "DELETE", &format!("/api/v4/channels/{mine}")).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        headers
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("rust")
    );
    let archived = latest_post(&rust, &mine).await;
    assert_eq!(archived["type"], "system_channel_deleted");
    let bot_id = archived["user_id"].as_str().expect("an author").to_owned();
    assert_eq!(archived["props"]["username"], "system-bot");
    assert_eq!(archived["message"], "system-bot archived the channel.");
    assert_eq!(
        archived["props"]["from_bot"], "true",
        "CreatePost marks a bot's post"
    );

    let (username, roles, first_name, is_bot_owner): (String, String, String, String) =
        sqlx::query_as(
            "SELECT u.username, u.roles, u.firstname, b.ownerid
               FROM users u JOIN bots b ON b.userid = u.id
              WHERE u.id = $1",
        )
        .bind(&bot_id)
        .fetch_one(&pool)
        .await
        .expect("the bot and its user exist");
    assert_eq!(username, "system-bot");
    assert_eq!(roles, "system_user");
    assert_eq!(
        first_name, "System",
        "UserFromBot puts the display name in FirstName"
    );
    let (first_admin,): (String,) = sqlx::query_as(
        "SELECT id FROM users WHERE roles LIKE '%system\\_admin%' ORDER BY username ASC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("an administrator");
    assert_eq!(
        is_bot_owner, first_admin,
        "owned by the first administrator by username"
    );
    // Then Go: `getOrCreateBot` finds ours by username and posts as it.
    common::invalidate_go_caches(&client, &token).await;
    let go = go_socket().expect("checked");
    let (status, _, body) = over_socket(&go, "DELETE", &format!("/api/v4/channels/{theirs}")).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let go_archived = latest_post(&go, &theirs).await;
    assert_same_notice(
        &go_archived,
        &archived,
        "the archive notice, Go reusing our bot",
    );
    assert_eq!(go_archived["user_id"], bot_id);
}
