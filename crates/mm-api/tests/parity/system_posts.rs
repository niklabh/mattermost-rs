//! Cross-server parity for the **system posts** twelve already-served route+method pairs write.
//!
//! These posts are invisible to every one of those routes' response bodies — Go logs and swallows
//! ten of the twelve, and marshals its answer from the channel it read *before* the post exists.
//! So this suite asserts the `Posts` rows themselves, read back through the server that wrote
//! them ([D-190]), plus the `posted` websocket event each one publishes.
//!
//! # Both servers get a channel with the same display name
//!
//! Three of the six lifecycle posts quote the channel's own text (`old_displayname`,
//! `old_header`, `old_purpose`), so a fixture whose two channels differed in those fields would
//! make a passing comparison impossible. [`twin_channels`] creates a pair that differs only in
//! `name`, which no system message names.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh -p mm-api --test parity system_post
//! ```

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, a_team_and_channel_the_user_is_in, client, create_plain_user,
    delete_channel, delete_plain_user, go_minted_token, stack_enabled,
};

/// The two channels one test writes to, one per server.
struct Twins {
    go_channel: String,
    rust_channel: String,
    /// How many system posts each channel carried before the test wrote anything.
    go_baseline: usize,
    rust_baseline: usize,
}

/// A pair of channels identical in everything a system message can quote.
///
/// `create_channel_typed` derives the display name from the tag, which would make the two
/// channels' display-name and header notices differ by construction — so this builds both here
/// with one display name, one header and one purpose.
async fn twin_channels(
    http: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    tag: &str,
    channel_type: &str,
) -> Twins {
    common::purge_api_fixtures().await;
    let mut ids = Vec::new();
    for side in ["go", "rust"] {
        let response = http
            .post(format!("{GO}/api/v4/channels"))
            .header("Authorization", format!("Bearer {admin_token}"))
            .json(&serde_json::json!({
                "team_id": team_id,
                "name": format!("mmrs-parity-{tag}-{side}"),
                "display_name": "mmrs parity twin",
                "header": "the original header",
                "purpose": "the original purpose",
                "type": channel_type,
            }))
            .send()
            .await
            .expect("Go answers");
        assert!(
            response.status().is_success(),
            "creating the twin channel failed: {}",
            response.text().await.unwrap_or_default()
        );
        let created: serde_json::Value = response.json().await.expect("the channel decodes");
        ids.push(created["id"].as_str().expect("an id").to_owned());
    }
    let mut twins = Twins {
        rust_channel: ids.pop().expect("two channels"),
        go_channel: ids.pop().expect("two channels"),
        go_baseline: 0,
        rust_baseline: 0,
    };
    // **Creating a channel already writes a system post.** `CreateChannelWithUser`
    // (app/channel.go:199) runs `postJoinChannelMessage` for the creator, so a fresh fixture
    // channel is not an empty timeline — and the fixture is built through Go on both sides, so
    // both channels carry one. Counted here rather than assumed, because the count is what every
    // test below slices off.
    twins.go_baseline = system_posts(http, GO, &twins.go_channel, admin_token)
        .await
        .len();
    twins.rust_baseline = system_posts(http, RUST, &twins.rust_channel, admin_token)
        .await
        .len();
    assert_eq!(
        twins.go_baseline, twins.rust_baseline,
        "the two fixture channels start with different timelines",
    );
    twins
}

/// One request, asserting a `RUST` answer really came from this port rather than the proxy.
async fn call(
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
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
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

/// The channel's posts, **oldest first**, as `(type, message, props)`.
///
/// Read through `base` and never through the other server: Go's caches do not see our writes and
/// ours do not see Go's ([D-190]). The id, the channel id and the two timestamps are dropped
/// rather than masked — every remaining field is compared exactly, and those four cannot agree
/// across two channels written a few milliseconds apart.
async fn system_posts(
    http: &reqwest::Client,
    base: &str,
    channel_id: &str,
    token: &str,
) -> Vec<serde_json::Value> {
    let (status, raw) = call(
        http,
        base,
        reqwest::Method::GET,
        &format!("/api/v4/channels/{channel_id}/posts?per_page=60"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{base} refused the post list: {raw}");
    let list: serde_json::Value = serde_json::from_str(raw.trim()).expect("a post list");
    let order = list["order"].as_array().cloned().unwrap_or_default();
    let mut out: Vec<serde_json::Value> = order
        .iter()
        .rev()
        .filter_map(|id| list["posts"].get(id.as_str()?))
        .map(|post| {
            serde_json::json!({
                "type": post["type"],
                "message": post["message"],
                "user_id": post["user_id"],
                "props": post["props"],
                "hashtags": post["hashtags"],
                "root_id": post["root_id"],
                "is_pinned": post["is_pinned"],
            })
        })
        .collect();
    out.retain(|post| {
        post["type"]
            .as_str()
            .is_some_and(|t| t.starts_with("system_"))
    });
    out
}

/// [`system_posts`] with the fixture's own creation posts dropped.
async fn system_posts_written_by_the_test(
    http: &reqwest::Client,
    base: &str,
    channel_id: &str,
    token: &str,
    baseline: usize,
) -> Vec<serde_json::Value> {
    let mut posts = system_posts(http, base, channel_id, token).await;
    assert!(
        posts.len() >= baseline,
        "{base}: the timeline lost a post it started with: {posts:#?}"
    );
    posts.drain(..baseline);
    posts
}

/// The channel row as each server answers it, for the counter assertions.
async fn channel_row(
    http: &reqwest::Client,
    base: &str,
    channel_id: &str,
    token: &str,
) -> serde_json::Value {
    let (status, raw) = call(
        http,
        base,
        reqwest::Method::GET,
        &format!("/api/v4/channels/{channel_id}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{base} refused the channel: {raw}");
    serde_json::from_str(raw.trim()).expect("a channel")
}

/// The four posts a membership change writes, and which of the four is decided by **who asked**.
///
/// Both servers are driven through the identical sequence — self-add, self-remove, add by the
/// admin, remove by the admin — against their own channel, and the two resulting post lists are
/// compared whole. A props key that is nearly right, a type swapped for its sibling, or an `@`
/// in the wrong place all show up here and in no response body.
#[tokio::test]
async fn the_four_membership_writes_post_what_go_posts() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let twins = twin_channels(&http, &admin, &team, "syspost-members", "O").await;
    let user = create_plain_user(&http, &admin, &team, "syspostm").await;

    for (base, channel) in [(GO, &twins.go_channel), (RUST, &twins.rust_channel)] {
        // A **self**-add: `opts.UserRequestorID == userID`, so `system_join_channel`.
        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/channels/{channel}/members"),
            &user.token,
            Some(&serde_json::json!({"user_id": user.id})),
        )
        .await;
        assert_eq!(status, 201, "{base} refused the self-add: {raw}");

        // A **self**-removal: `system_leave_channel`.
        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::DELETE,
            &format!("/api/v4/channels/{channel}/members/{}", user.id),
            &user.token,
            None,
        )
        .await;
        assert_eq!(status, 200, "{base} refused the self-removal: {raw}");

        // Added by somebody else: `system_add_to_channel`, with four props.
        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/channels/{channel}/members"),
            &admin,
            Some(&serde_json::json!({"user_id": user.id})),
        )
        .await;
        assert_eq!(status, 201, "{base} refused the add: {raw}");

        // Removed by somebody else: `system_remove_from_channel`, and it names only the removed.
        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::DELETE,
            &format!("/api/v4/channels/{channel}/members/{}", user.id),
            &admin,
            None,
        )
        .await;
        assert_eq!(status, 200, "{base} refused the removal: {raw}");
    }

    let go_posts =
        system_posts_written_by_the_test(&http, GO, &twins.go_channel, &admin, twins.go_baseline)
            .await;
    let rust_posts = system_posts_written_by_the_test(
        &http,
        RUST,
        &twins.rust_channel,
        &admin,
        twins.rust_baseline,
    )
    .await;

    let types: Vec<&str> = rust_posts
        .iter()
        .filter_map(|post| post["type"].as_str())
        .collect();
    assert_eq!(
        types,
        vec![
            "system_join_channel",
            "system_leave_channel",
            "system_add_to_channel",
            "system_remove_from_channel",
        ],
        "the four membership posts, in the order the writes happened: {rust_posts:#?}"
    );
    assert_eq!(
        go_posts, rust_posts,
        "the membership system posts differ\n go: {go_posts:#?}\nrust: {rust_posts:#?}"
    );

    // The two the client actually renders from, spelled out — `props` is the whole contract.
    let add = &rust_posts[2];
    assert_eq!(add["props"]["addedUserId"], serde_json::json!(user.id));
    assert!(
        add["props"].get("username").is_some() && add["props"].get("userId").is_some(),
        "the add post names the adder as well as the added: {add:#?}"
    );
    let removed = &rust_posts[3];
    assert_eq!(
        removed["props"]["removedUserId"],
        serde_json::json!(user.id)
    );
    assert!(
        removed["props"].get("username").is_none(),
        "`postRemoveFromChannelMessage` has no `username` prop: {removed:#?}"
    );

    delete_plain_user(&http, &admin, &user.id).await;
    delete_channel(&http, &admin, &twins.go_channel).await;
    delete_channel(&http, &admin, &twins.rust_channel).await;
}

/// A join post moves the channel but does not make it unread, and a real post does both.
///
/// `SaveMultiple`'s post-commit `UPDATE Channels` sets `LastPostAt` unconditionally and adds
/// `count` to `TotalMsgCount`, where `count` is zero for a join/leave message. Dropping the guard
/// makes every join unread; dropping the whole statement leaves the channel sorted where it was.
/// Both mistakes are invisible to the route that made the post, so the counters are read from the
/// channel afterwards.
#[tokio::test]
async fn a_join_post_moves_last_post_at_without_counting_as_a_message() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let twins = twin_channels(&http, &admin, &team, "syspost-count", "O").await;
    let user = create_plain_user(&http, &admin, &team, "syspostc").await;

    for (base, channel) in [(GO, &twins.go_channel), (RUST, &twins.rust_channel)] {
        let before = channel_row(&http, base, channel, &admin).await;
        // Not zero: the creator's own join post moved `LastPostAt` already. The count *is* zero,
        // because that post was a join too.
        assert_eq!(before["total_msg_count"], 0, "{base}: no message yet");
        let last_post_at_before = before["last_post_at"].as_i64().unwrap_or(0);

        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/channels/{channel}/members"),
            &user.token,
            Some(&serde_json::json!({"user_id": user.id})),
        )
        .await;
        assert_eq!(status, 201, "{base} refused the self-add: {raw}");

        let after = channel_row(&http, base, channel, &admin).await;
        assert_eq!(
            after["total_msg_count"], 0,
            "{base}: a join is not a message — `IsJoinLeaveMessage` keeps the count still",
        );
        assert_eq!(
            after["total_msg_count_root"], 0,
            "{base}: and neither is it a root message",
        );
        assert!(
            after["last_post_at"].as_i64().unwrap_or(0) > last_post_at_before,
            "{base}: `LastPostAt` moves anyway, which is what reorders the sidebar: {after}",
        );
        assert_eq!(
            after["last_post_at"], after["last_root_post_at"],
            "{base}: a root post moves both dates together",
        );

        // The contrast: an ordinary message *is* counted, so the guard above is not simply a
        // statement that never runs.
        //
        // Posted as the **admin**, not as the user who just joined. `post_message` goes to Go,
        // and Go's channel-member cache has not seen a membership mm-api wrote — so posting as
        // the joiner is a 403 from Go on the Rust half of this loop. That is [D-190] exactly, and
        // it cost this test one run.
        common::post_message(&http, &admin, channel, "an ordinary message", None).await;
        let posted = channel_row(&http, base, channel, &admin).await;
        assert_eq!(
            posted["total_msg_count"], 1,
            "{base}: an ordinary post counts: {posted}",
        );
    }

    delete_plain_user(&http, &admin, &user.id).await;
    delete_channel(&http, &admin, &twins.go_channel).await;
    delete_channel(&http, &admin, &twins.rust_channel).await;
}

/// The six posts the channel-lifecycle writes owe, in the order the routes are called.
///
/// The patch writes three of them — display name, header, purpose, each guarded on its own field
/// and in that order — then a privacy conversion, an archive and a restore. The archive and
/// restore posts land in a channel that is, at that moment, archived; they are read back after
/// the restore.
#[tokio::test]
async fn the_channel_lifecycle_writes_post_what_go_posts() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let twins = twin_channels(&http, &admin, &team, "syspost-life", "O").await;

    for (base, channel) in [(GO, &twins.go_channel), (RUST, &twins.rust_channel)] {
        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::PUT,
            &format!("/api/v4/channels/{channel}/patch"),
            &admin,
            Some(&serde_json::json!({
                "display_name": "mmrs parity renamed",
                "header": "the new header",
                "purpose": "",
            })),
        )
        .await;
        assert_eq!(status, 200, "{base} refused the patch: {raw}");

        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::PUT,
            &format!("/api/v4/channels/{channel}/privacy"),
            &admin,
            Some(&serde_json::json!({"privacy": "P"})),
        )
        .await;
        assert_eq!(status, 200, "{base} refused the privacy change: {raw}");

        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::DELETE,
            &format!("/api/v4/channels/{channel}"),
            &admin,
            None,
        )
        .await;
        assert_eq!(status, 200, "{base} refused the archive: {raw}");

        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/channels/{channel}/restore"),
            &admin,
            None,
        )
        .await;
        assert_eq!(status, 200, "{base} refused the restore: {raw}");
    }

    let go_posts =
        system_posts_written_by_the_test(&http, GO, &twins.go_channel, &admin, twins.go_baseline)
            .await;
    let rust_posts = system_posts_written_by_the_test(
        &http,
        RUST,
        &twins.rust_channel,
        &admin,
        twins.rust_baseline,
    )
    .await;

    let types: Vec<&str> = rust_posts
        .iter()
        .filter_map(|post| post["type"].as_str())
        .collect();
    assert_eq!(
        types,
        vec![
            "system_displayname_change",
            "system_header_change",
            "system_purpose_change",
            "system_change_chan_privacy",
            "system_channel_deleted",
            "system_channel_restored",
        ],
        "the six lifecycle posts, display name before header before purpose: {rust_posts:#?}"
    );
    assert_eq!(
        go_posts, rust_posts,
        "the lifecycle system posts differ\n go: {go_posts:#?}\nrust: {rust_posts:#?}"
    );

    // The purpose notice took the **removed** branch, which is the middle of the three and the
    // one a reader is most likely to swap with `updated_to`.
    let purpose = &rust_posts[2];
    assert_eq!(purpose["props"]["new_purpose"], serde_json::json!(""));
    assert_eq!(
        purpose["props"]["old_purpose"],
        serde_json::json!("the original purpose")
    );
    assert!(
        purpose["message"]
            .as_str()
            .is_some_and(|m| m.contains("removed the channel purpose (was:")),
        "the empty-new branch: {purpose:#?}"
    );
    // The header notice took `updated_from`, because both sides were non-empty.
    assert!(
        rust_posts[1]["message"]
            .as_str()
            .is_some_and(|m| m.contains("updated the channel header from:")),
        "the both-non-empty branch: {:#?}",
        rust_posts[1]
    );
    // The privacy sentence follows the new type and names nobody.
    assert_eq!(
        rust_posts[3]["message"],
        serde_json::json!("This channel has been converted to a Private Channel.")
    );

    delete_channel(&http, &admin, &twins.go_channel).await;
    delete_channel(&http, &admin, &twins.rust_channel).await;
}

/// `updateChannel` compares the old display name against the **submitted** one, not the applied
/// one — so a body that omits `display_name` changes nothing and still posts, with an empty new
/// value. A port that compared against the written channel would post nothing here.
#[tokio::test]
async fn update_channel_posts_the_display_name_it_was_sent() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let twins = twin_channels(&http, &admin, &team, "syspost-upd", "O").await;

    for (base, channel) in [(GO, &twins.go_channel), (RUST, &twins.rust_channel)] {
        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::PUT,
            &format!("/api/v4/channels/{channel}"),
            &admin,
            // No `display_name`, so `apply_update` leaves the stored one alone.
            Some(&serde_json::json!({"id": channel, "header": "another header"})),
        )
        .await;
        assert_eq!(status, 200, "{base} refused the update: {raw}");

        let stored = channel_row(&http, base, channel, &admin).await;
        assert_eq!(
            stored["display_name"], "mmrs parity twin",
            "{base}: the stored display name did not change",
        );
    }

    let go_posts =
        system_posts_written_by_the_test(&http, GO, &twins.go_channel, &admin, twins.go_baseline)
            .await;
    let rust_posts = system_posts_written_by_the_test(
        &http,
        RUST,
        &twins.rust_channel,
        &admin,
        twins.rust_baseline,
    )
    .await;
    assert_eq!(
        go_posts, rust_posts,
        "the display-name notice differs\n go: {go_posts:#?}\nrust: {rust_posts:#?}"
    );
    assert_eq!(rust_posts.len(), 1, "one post: {rust_posts:#?}");
    assert_eq!(rust_posts[0]["type"], "system_displayname_change");
    assert_eq!(
        rust_posts[0]["props"]["new_displayname"],
        serde_json::json!("")
    );
    assert_eq!(
        rust_posts[0]["props"]["old_displayname"],
        serde_json::json!("mmrs parity twin"),
    );
    // `updateChannel` writes no header notice at all — that one belongs to `patchChannel`.
    assert!(
        rust_posts
            .iter()
            .all(|post| post["type"] != "system_header_change"),
        "updateChannel posts only the display-name notice: {rust_posts:#?}"
    );

    delete_channel(&http, &admin, &twins.go_channel).await;
    delete_channel(&http, &admin, &twins.rust_channel).await;
}

/// Every system post publishes a `posted` event, and its six data fields are the contract.
///
/// The probe is the admin's, who is in the channel; the joining user's own socket would also see
/// the event, but the admin's is the one that proves the broadcast is channel-addressed rather
/// than user-addressed.
#[tokio::test]
async fn a_membership_system_post_publishes_a_posted_event() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let twins = twin_channels(&http, &admin, &team, "syspost-ws", "O").await;
    let user = create_plain_user(&http, &admin, &team, "syspostw").await;

    let mut frames = Vec::new();
    for (base, channel) in [(GO, &twins.go_channel), (RUST, &twins.rust_channel)] {
        let mut probe = SocketProbe::connect(base, &admin).await;

        let (status, raw) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/channels/{channel}/members"),
            &user.token,
            Some(&serde_json::json!({"user_id": user.id})),
        )
        .await;
        assert_eq!(status, 201, "{base} refused the self-add: {raw}");

        // **Match on the channel as well as the type.** A socket is not isolated: a sibling test
        // joining its own channel puts a `system_join_channel` frame on this connection too, and
        // a predicate that only looked at the type picked whichever arrived first. That is the
        // whole-suite failure this test had on its first full run.
        let wanted_channel = channel.clone();
        let arrived = probe
            .collect_until(Duration::from_secs(8), move |collected| {
                collected.iter().any(|frame| {
                    frame.get("event").and_then(|e| e.as_str()) == Some("posted")
                        && frame["data"]["post"].as_str().is_some_and(|p| {
                            p.contains("system_join_channel") && p.contains(&wanted_channel)
                        })
                })
            })
            .await;
        assert!(
            arrived,
            "{base} published no `posted` event for the join: {:?}",
            probe.raw
        );

        let frame = probe
            .events_named("posted")
            .into_iter()
            .find(|frame| {
                frame["data"]["post"]
                    .as_str()
                    .is_some_and(|p| p.contains("system_join_channel") && p.contains(channel))
            })
            .expect("the join's posted event");
        let data = frame["data"].as_object().expect("a data object").clone();
        let post: serde_json::Value =
            serde_json::from_str(data["post"].as_str().expect("a post string"))
                .expect("the post decodes");

        frames.push(serde_json::json!({
            "channel_type": data.get("channel_type"),
            "channel_display_name": data.get("channel_display_name"),
            "channel_name_is_the_channels": data
                .get("channel_name")
                .and_then(|n| n.as_str())
                .is_some_and(|n| n.starts_with("mmrs-parity-syspost-ws-")),
            // `GetSenderName` short-circuits on `IsSystemMessage`, so this is never a username.
            "sender_name": data.get("sender_name"),
            "team_id": data.get("team_id"),
            "set_online": data.get("set_online"),
            "post_type": post["type"].clone(),
            "post_props": post["props"].clone(),
            "broadcast_channel_id_is_set": frame["broadcast"]["channel_id"]
                .as_str()
                .is_some_and(|c| !c.is_empty()),
            "broadcast_user_id": frame["broadcast"]["user_id"].clone(),
        }));
    }

    assert_eq!(
        frames[0], frames[1],
        "the `posted` events differ\n go: {:#?}\nrust: {:#?}",
        frames[0], frames[1]
    );
    assert_eq!(frames[1]["sender_name"], serde_json::json!("System"));
    assert_eq!(frames[1]["set_online"], serde_json::json!(true));
    assert_eq!(frames[1]["team_id"], serde_json::json!(team));
    assert_eq!(frames[1]["broadcast_user_id"], serde_json::json!(""));
    assert_eq!(
        frames[1]["broadcast_channel_id_is_set"],
        serde_json::json!(true)
    );

    delete_plain_user(&http, &admin, &user.id).await;
    delete_channel(&http, &admin, &twins.go_channel).await;
    delete_channel(&http, &admin, &twins.rust_channel).await;
}
