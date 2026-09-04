//! Cross-server parity for `GET /api/v4/channels/{channel_id}/pinned`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity channel_pinned
//! ```
//!
//! # The list is oldest-first, and it is the only post list in the port that is
//!
//! `SqlChannelStore.GetPinnedPosts` orders `CreateAt ASC` where every other post query orders
//! `DESC`. `order` is on the wire, so the direction is wire format; the fixture pins three posts
//! in a known sequence precisely so a flipped comparison fails rather than ties.
//!
//! # `read_channel_content`, not `read_channel`
//!
//! The same channel, refused by the same underlying check, reports a different permission here
//! than `getChannel` does one route over. The suite asserts the refusal against a non-member.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_channel_typed, create_plain_user, delete_post, fetch_both,
    fetch_both_raw, go_minted_token, logged_in_user_id, pin_post, post_message, purge_api_fixtures,
    stack_enabled,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

struct Fixture {
    /// Three pinned posts and one unpinned, plus a reply so `reply_count` is not all zeroes.
    channel_id: String,
    first_pinned: String,
    second_pinned: String,
    third_pinned: String,
    unpinned: String,
    /// Pinned and then deleted: the outer query's `DeleteAt = 0` is the only thing hiding it.
    deleted_pin: String,
    /// A channel with nothing pinned at all.
    empty_channel_id: String,
    /// A **private** channel with one pinned post. A public one cannot test a refusal: the
    /// `read_public_channel` fallback serves any team member, which the outsider is.
    private_channel_id: String,
    /// A non-admin who is in the team and in none of these channels.
    outsider_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, token).await;
            let channel_id = create_channel(client, token, &team_id, "pinned").await;
            add_user_to_channel(client, token, &channel_id, logged_in_user_id()).await;

            // Created in order, so `CreateAt ASC` and `DESC` are distinguishable. Posting is
            // sequential and Go stamps `CreateAt` from the clock, so the sequence is the
            // creation order — but two posts *can* land in the same millisecond, which would
            // tie the sort and make the direction unobservable. The reply between each pair is
            // what keeps them apart, and it earns its place twice: it also makes the root's
            // `reply_count` non-zero, so a dropped subquery is visible.
            let first_pinned = post_message(client, token, &channel_id, "pin one", None).await;
            post_message(client, token, &channel_id, "a reply", Some(&first_pinned)).await;
            let second_pinned = post_message(client, token, &channel_id, "pin two", None).await;
            post_message(
                client,
                token,
                &channel_id,
                "another reply",
                Some(&second_pinned),
            )
            .await;
            let third_pinned = post_message(client, token, &channel_id, "pin three", None).await;
            let unpinned = post_message(client, token, &channel_id, "not pinned", None).await;

            for id in [&first_pinned, &second_pinned, &third_pinned] {
                pin_post(client, token, id).await;
            }

            // A **deleted** pinned post and a **deleted** reply. Both queries filter
            // `DeleteAt = 0` — the outer one and the reply-count subquery — and neither filter
            // is observable without a row it has to exclude. Without these two the predicates
            // could be deleted outright and every assertion in this file would still pass.
            let deleted_pin =
                post_message(client, token, &channel_id, "pinned then deleted", None).await;
            pin_post(client, token, &deleted_pin).await;
            delete_post(client, token, &deleted_pin).await;

            let deleted_reply = post_message(
                client,
                token,
                &channel_id,
                "reply then deleted",
                Some(&first_pinned),
            )
            .await;
            delete_post(client, token, &deleted_reply).await;

            let empty_channel_id = create_channel(client, token, &team_id, "pinnedempty").await;
            add_user_to_channel(client, token, &empty_channel_id, logged_in_user_id()).await;

            let private_channel_id =
                create_channel_typed(client, token, &team_id, "pinnedpriv", "P").await;
            add_user_to_channel(client, token, &private_channel_id, logged_in_user_id()).await;
            let private_post =
                post_message(client, token, &private_channel_id, "secret pin", None).await;
            pin_post(client, token, &private_post).await;

            let outsider = create_plain_user(client, token, &team_id, "pinned").await;

            Fixture {
                channel_id,
                first_pinned,
                second_pinned,
                third_pinned,
                unpinned,
                deleted_pin,
                empty_channel_id,
                private_channel_id,
                outsider_token: outsider.token,
            }
        })
        .await
}

// ---------------------------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_pinned_list_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/pinned", f.channel_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert!(
        go.ends_with(b"\n"),
        "PostList.EncodeJSON appends the newline json.Encoder writes"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    for key in [
        "order",
        "posts",
        "next_post_id",
        "prev_post_id",
        "first_inaccessible_post_time",
    ] {
        assert!(parsed.get(key).is_some(), "{key} must be on the wire");
    }
    let order: Vec<&str> = parsed["order"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|v| v.as_str().expect("an id"))
        .collect();
    assert_eq!(order.len(), 3, "three pinned, one not");
    assert!(
        !order.contains(&f.unpinned.as_str()),
        "IsPinned = true is a predicate, not a decoration"
    );
    assert!(
        !order.contains(&f.deleted_pin.as_str()),
        "and so is DeleteAt = 0 — a pinned post that was later deleted is gone"
    );
}

/// `CreateAt ASC`, the reverse of every other post list. A port that reused
/// `getPostsForChannel`'s ordering passes every other assertion in this file and fails this one.
#[tokio::test]
async fn the_order_is_oldest_first() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/pinned", f.channel_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs);

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let order: Vec<&str> = parsed["order"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|v| v.as_str().expect("an id"))
        .collect();
    assert_eq!(
        order,
        vec![
            f.first_pinned.as_str(),
            f.second_pinned.as_str(),
            f.third_pinned.as_str()
        ],
        "oldest first"
    );

    // And the timestamps really are distinct, or the assertion above would hold for a tie.
    let posts = &parsed["posts"];
    let stamps: Vec<i64> = order
        .iter()
        .map(|id| posts[*id]["create_at"].as_i64().expect("a timestamp"))
        .collect();
    assert!(
        stamps[0] < stamps[1] && stamps[1] < stamps[2],
        "the fixture's three pins must not share a millisecond: {stamps:?}"
    );
}

/// The `ReplyCount` subquery is unconditional here — there is no `skip_fetch_threads` — so a
/// pinned root reports its thread's live count.
#[tokio::test]
async fn a_pinned_root_carries_its_reply_count() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/pinned", f.channel_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(go, rs);

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed["posts"][&f.first_pinned]["reply_count"], 1,
        "one live reply on the first pin — the second was deleted, and the subquery's own \
         `DeleteAt = 0` is what keeps it out"
    );
    assert_eq!(
        parsed["posts"][&f.third_pinned]["reply_count"], 0,
        "and none on the third — two equal counters could not catch a wrong column"
    );
}

/// `NewPostList` materialises both collections, and nothing here calls `MakeNonNil`, so an empty
/// answer is `[]`/`{}` rather than two `null`s.
#[tokio::test]
async fn a_channel_with_nothing_pinned_answers_empty_collections() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/pinned", f.empty_channel_id);
    let (go, rs) = fetch_both(&client, &token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed["order"], serde_json::json!([]));
    assert_eq!(parsed["posts"], serde_json::json!({}));
}

/// `PostList.Etag()` is `<version>.<len(order)>.<max updateAt>.<max deleteAt>`, so — unlike
/// `GetEtagForFileInfos` — it is stable for an empty list and a 304 is reachable either way.
#[tokio::test]
async fn the_pinned_etag_agrees_and_round_trips() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for channel in [&f.channel_id, &f.empty_channel_id] {
        let path = format!("/api/v4/channels/{channel}/pinned");
        let mut etags = Vec::new();
        for base in [GO, RUST] {
            let response = client
                .get(format!("{base}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("reachable");
            assert_eq!(response.status(), 200);
            etags.push(
                response
                    .headers()
                    .get("ETag")
                    .expect("an ETag")
                    .to_str()
                    .expect("ASCII")
                    .to_owned(),
            );
        }
        assert_eq!(etags[0], etags[1], "{path}: the two etags must agree");

        for base in [GO, RUST] {
            let response = client
                .get(format!("{base}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .header("If-None-Match", &etags[0])
                .send()
                .await
                .expect("reachable");
            assert_eq!(response.status(), 304, "{base}{path} honours the etag");
            assert_eq!(
                response.headers().get("ETag").and_then(|v| v.to_str().ok()),
                Some(etags[0].as_str())
            );
        }
    }
}

/// The permission the refusal names, which is *not* the one `getChannel` names for the same
/// channel and the same user — and, one assertion later, the fallback that makes a *public*
/// channel's pins readable by any team member without a membership row.
#[tokio::test]
async fn an_outsider_is_refused_a_private_channel_and_served_a_public_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/channels/{}/pinned", f.private_channel_id);
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.outsider_token, &path).await;
    assert_eq!(go_status, 403, "no membership, no read_public_channel");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "api.context.permissions.app_error");

    // The public channel's pins, on the other hand, are readable by any member of its team —
    // `HasPermissionToReadChannel`'s open-channel fallback. Asserting only the refusal above
    // would pass on a port that refused everybody.
    let public = format!("/api/v4/channels/{}/pinned", f.channel_id);
    let (go, rs) = fetch_both(&client, &f.outsider_token, &public).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{public}: a team member reads a public channel's pins without joining it"
    );
}

/// The channel is fetched **before** the permission check, so an unknown id is a 404 for
/// everybody — an outsider included, which is a disclosure both servers make identically.
#[tokio::test]
async fn an_unknown_channel_is_a_404_for_member_and_outsider_alike() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = "/api/v4/channels/zzzzzzzzzzzzzzzzzzzzzzzzzz/pinned";
    for actor in [token.as_str(), f.outsider_token.as_str()] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, actor, path).await;
        assert_eq!(go_status, 404, "the channel lookup runs first");
        assert_eq!(rs_status, go_status);
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    }
}

#[tokio::test]
async fn a_malformed_channel_id_is_a_400_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let path = "/api/v4/channels/short/pinned";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, path).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
}

/// The five writes and the deeper reads under `/channels/{id}/` are unregistered and must still
/// be Go's — a claim about the *router*, so it is made over HTTP.
#[tokio::test]
async fn the_neighbouring_channel_routes_are_still_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for suffix in ["pinned/", "timezones", "moderations"] {
        let path = format!("/api/v4/channels/{}/{suffix}", f.channel_id);
        let response = client
            .get(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("reachable");
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("go"),
            "{path} must still reach the Go server"
        );
    }

    // And a non-GET on the exact migrated path falls to the method fallback.
    let path = format!("/api/v4/channels/{}/pinned", f.channel_id);
    let response = client
        .post(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("reachable");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "only GET is migrated"
    );
}
