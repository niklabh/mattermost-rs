//! Cross-server parity for the four per-user post writes under
//! `/api/v4/users/{user_id}/posts/{post_id}/`.
//!
//! ```sh
//! MMRS_STACK=4 scripts/parity.sh -p mm-api --test parity post_acks
//! ```
//!
//! # Three shapes in one family
//!
//! | route | what this server does |
//! |---|---|
//! | `POST   …/ack`        | refuses, 501 `<untranslated>` |
//! | `DELETE …/ack`        | refuses, 501 `license_error.feature_unavailable` |
//! | `POST   …/set_unread` | serves a DM/GM root or CRT-supported post; forwards the rest |
//! | `POST   …/reminder`   | forwards — not registered at all ([D-420]) |
//!
//! The two `/ack` halves are the interesting pair: same path, same gate, same status, same
//! (wiped) detail, **different `id`**. Go's own two responses are compared to each other here,
//! not merely to ours, so the divergence is established from the oracle rather than from reading
//! `api4/post.go`.
//!
//! # Why the fixture is a DM with four posts in it
//!
//! `set_unread`'s body is `model.ChannelUnreadAt`: five counters and a timestamp. A channel with
//! three plain root posts in it makes `msg_count == msg_count_root` and both mention counters
//! equal, so a port that read the wrong column compares equal to Go anyway. The fixture below is
//! built so that **no two of the four counters hold the same number** — see [`fixture`].
//!
//! # These are writes, and running them twice is the point
//!
//! `post_both_raw` posts to Go and then to us, so our call sees the state Go's call left. That is
//! safe here and deliberately so: `UpdateLastViewedAtPost` writes `LastViewedAt` from the
//! **post's** `CreateAt - 1` and both message counts from `Channels.TotalMsgCount*` minus a count
//! over `Posts` — every input is a post row, none is the member row being overwritten. So the
//! operation is idempotent and the second caller must produce the first caller's answer. A future
//! change that made any of it depend on the member's prior state would turn this suite red, which
//! is the correct outcome.

use crate::common;

use common::{
    GO, RUST, a_team_and_channel_the_user_is_in, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel, create_direct_channel,
    create_plain_user, delete_plain_user, go_minted_token, logged_in_user_id, post_message,
    stack_enabled,
};

/// A 26-character id that names nothing.
const ABSENT_ID: &str = "mmrsnosuchpostmmrsnosuchp1";

// Every fixture tag in this module starts `pa` — **post acks** — because
// `common::create_plain_user` turns a tag into a username (`mmrsplain{tag}`) and usernames are
// unique across the whole binary. The first version of this file used `unreadbody`, which
// `parity/channel_unread` had already claimed: the two tests passed alone and one of them failed
// with `app.user.save.username_exists.app_error` in every concurrent run. A tag is a global
// name, not a local one.

/// POST to both servers **without** asserting the Rust side served it.
///
/// `common::post_both_raw` asserts `x-mmrs-served-by: rust`, which is right for a route this server
/// answers and wrong for one it deliberately hands to Go. Returns the header alongside the
/// status and body so a test can assert *which* server answered rather than assuming.
async fn post_both_allowing_forward(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, Vec<u8>, bool)) {
    let go = {
        let response = client
            .post(format!("{GO}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .unwrap_or_else(|e| panic!("{GO}{path} is unreachable: {e}"));
        (
            response.status().as_u16(),
            response.bytes().await.expect("body reads").to_vec(),
        )
    };
    let ours = {
        let response = client
            .post(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .unwrap_or_else(|e| panic!("{RUST}{path} is unreachable: {e}"));
        let served_here = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            == Some("rust");
        (
            response.status().as_u16(),
            response.bytes().await.expect("body reads").to_vec(),
            served_here,
        )
    };
    (go, ours)
}

/// DELETE on both servers, returning `(status, body)` from each and whether we answered.
async fn delete_both_allowing_forward(
    client: &reqwest::Client,
    token: &str,
    path: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>, bool)) {
    let go = {
        let response = client
            .delete(format!("{GO}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{GO}{path} is unreachable: {e}"));
        (
            response.status().as_u16(),
            response.bytes().await.expect("body reads").to_vec(),
        )
    };
    let ours = {
        let response = client
            .delete(format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{RUST}{path} is unreachable: {e}"));
        let served_here = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            == Some("rust");
        (
            response.status().as_u16(),
            response.bytes().await.expect("body reads").to_vec(),
            served_here,
        )
    };
    (go, ours)
}

/// A post's `create_at`, read back through Go.
///
/// Needed because `LastViewedAt` is written as the **post's** `CreateAt - 1` and nothing else on
/// the wire says what that value should be. Asserting only that it is non-zero leaves
/// `lastviewedat = :lastviewedat` and `lastviewedat = :updatedat` indistinguishable — both are
/// large positive millisecond timestamps.
async fn post_create_at(client: &reqwest::Client, token: &str, post_id: &str) -> i64 {
    let body: serde_json::Value = client
        .get(format!("{GO}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the post decodes");
    body["create_at"].as_i64().expect("a create_at")
}

/// `id` out of an error body, or `None` when the body is not an `AppError`.
fn error_id(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("id")?
        .as_str()
        .map(str::to_owned)
}

/// A direct-message channel between the admin and a fresh plain user, with posts arranged so
/// that the four counters `ChannelUnreadAt` carries all hold **different** numbers.
///
/// # The algebra the layout is solving
///
/// With `pr`/`pp` root and reply posts *before* the marked post, and a window that holds `wr`
/// roots and `wp` replies of which `mr`/`mp` are the reader's own:
///
/// | field | value |
/// |---|---|
/// | `msg_count` | `pr + pp` — every post before the window, mine included |
/// | `msg_count_root` | `pr` — the roots before the window |
/// | `mention_count` | `(wr + wp) - (mr + mp)` — window posts somebody **else** wrote |
/// | `mention_count_root` | `wr - mr` — window roots somebody else wrote |
///
/// Two separate exclusions fall out of that, and each is one word in the port:
/// `update_last_viewed_at_post` passes an **empty** excluded user, so the reader's own posts
/// count towards the message totals; `count_mentions_from_post` passes the **reader's id**, so
/// they do not count towards the mentions. Swapping the two is invisible unless the reader has
/// posted inside the window, which is why `C` exists.
///
/// The layout below sets `pr = 5`, `pp = 2`, and a window of `A` (admin root, the mark point),
/// `B` and `E` (admin replies to A), `C` (the reader's own root) and `D` (a final admin root) —
/// giving `7, 5, 4, 2` when marking from `A`. `D` is what makes `mention_count_root` non-zero
/// for a window starting at `B`, which is what makes the CRT dispatcher observable at all.
///
/// A first attempt without the pre-window posts gave `0, 0, 3, 2`: both message counts were
/// `Total - unread` over a channel where the window *was* everything, so a port that read
/// `TotalMsgCountRoot` for both compared equal to Go. That is the tie this layout breaks.
struct Fixture {
    /// The DM. Kept so a future test can read it back through `getChannelUnread`.
    #[allow(dead_code)]
    channel_id: String,
    reader: common::PlainUser,
    /// A, the root every "mark from here" test uses.
    root_post: String,
    /// B, a reply to A — the post whose CRT arm differs.
    reply_post: String,
    /// A genuinely **open** channel the reader is in, for the forwarded branch.
    ///
    /// Created rather than discovered: `a_team_and_channel_the_user_is_in` returns the caller's
    /// first channel, and on this stack that is a `D`. A "public channel" test built on it was
    /// served by the DM branch and passed for the wrong reason.
    public_channel: String,
}

async fn fixture(client: &reqwest::Client, admin_token: &str, tag: &str) -> Fixture {
    let (team_id, _) = a_team_and_channel_the_user_is_in(client, admin_token).await;
    let reader = create_plain_user(client, admin_token, &team_id, tag).await;

    let public_channel = create_channel(client, admin_token, &team_id, tag).await;
    add_user_to_channel(client, admin_token, &public_channel, &reader.id).await;

    let channel_id =
        create_direct_channel(client, admin_token, logged_in_user_id(), &reader.id).await;

    // Before the window: five roots and two replies. `pr = 5`, `pp = 2`.
    let mut pre_root = String::new();
    for n in 0..5 {
        pre_root = post_message(
            client,
            admin_token,
            &channel_id,
            &format!("mmrs pre root {n}"),
            None,
        )
        .await;
    }
    for n in 0..2 {
        post_message(
            client,
            admin_token,
            &channel_id,
            &format!("mmrs pre reply {n}"),
            Some(&pre_root),
        )
        .await;
    }

    // The window: A, then two replies by the admin, then one root by the reader.
    let root_post = post_message(client, admin_token, &channel_id, "mmrs ack root", None).await;
    let reply_post = post_message(
        client,
        admin_token,
        &channel_id,
        "mmrs ack reply",
        Some(&root_post),
    )
    .await;
    post_message(
        client,
        admin_token,
        &channel_id,
        "mmrs ack reply two",
        Some(&root_post),
    )
    .await;
    post_message(client, &reader.token, &channel_id, "mmrs ack mine", None).await;
    // `D`, an admin **root** after the reader's own. Without it every window that starts at `B`
    // holds no root the reader did not write, both CRT arms answer `mention_count_root: 0`, and
    // the dispatcher that chooses between them is untested — which is exactly how the first
    // version of this fixture passed the served arm and proved nothing about it.
    post_message(client, admin_token, &channel_id, "mmrs ack last", None).await;

    Fixture {
        channel_id,
        reader,
        root_post,
        reply_post,
        public_channel,
    }
}

async fn unwind(client: &reqwest::Client, admin_token: &str, fixture: Fixture) {
    // The DM channel itself is left: Go has no route that removes one, and `purge_api_fixtures`
    // sweeps `mmrsplain%` rows on the next run. Deleting the user is what makes the channel inert.
    delete_plain_user(client, admin_token, &fixture.reader.id).await;
}

/// The two `/ack` halves both refuse with **501**, and Go's own two ids differ.
///
/// The second assertion is the one that matters: it establishes from the running Go server that
/// `POST` and `DELETE` on the same path do not share an error id. Everything this port does with
/// those two constants rests on it, and reading `api4/post.go` is not evidence.
#[tokio::test]
async fn the_two_ack_halves_refuse_with_different_ids() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let path = format!("/api/v4/users/{me}/posts/{ABSENT_ID}/ack");

    let (go_post, ours_post) = post_both_allowing_forward(&client, &token, &path, b"{}").await;
    let (go_delete, ours_delete) = delete_both_allowing_forward(&client, &token, &path).await;

    assert_eq!(go_post.0, 501, "Go refuses the POST for the licence");
    assert_eq!(go_delete.0, 501, "Go refuses the DELETE for the licence");
    assert!(ours_post.2, "the POST is answered here, not forwarded");
    assert!(ours_delete.2, "the DELETE is answered here, not forwarded");

    let go_post_id = error_id(&go_post.1).expect("Go's POST body is an AppError");
    let go_delete_id = error_id(&go_delete.1).expect("Go's DELETE body is an AppError");
    assert_ne!(
        go_post_id, go_delete_id,
        "the oracle says the two halves carry different ids; if this ever fails, one of the two \
         constants in `licensed_features.rs` has to change with it"
    );
    assert_eq!(go_post_id, "<untranslated>");
    assert_eq!(go_delete_id, "license_error.feature_unavailable");

    assert_eq!(ours_post.0, go_post.0, "POST …/ack status");
    assert_eq!(ours_delete.0, go_delete.0, "DELETE …/ack status");
    // `request_id` is minted per request and can never match; `message` is Go's translation of
    // the id. For the POST the id **is** `<untranslated>`, whose translation is itself, so the
    // messages agree too and the helper's tolerance goes unused there.
    assert_error_bodies_match_except_known_gaps(&go_post.1, &ours_post.1, "POST …/ack");
    assert_error_bodies_match_except_known_gaps(&go_delete.1, &ours_delete.1, "DELETE …/ack");
}

/// The licence test is the **first statement**, so nothing else about the request is consulted.
///
/// Each row below would be refused by a *different* check if the licence test were not above it:
/// a `{post_id}` Go's mux accepts but `RequirePostId` rejects (400), a `{user_id}` naming somebody
/// the caller may not act for (403), and a post in a channel the caller cannot read (403). All
/// three answer 501 on both servers.
#[tokio::test]
async fn nothing_else_about_an_ack_request_is_ever_consulted() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let fixture = fixture(&client, &token, "packorder").await;

    for (why, path) in [
        (
            "a post_id Go's mux accepts and RequirePostId rejects",
            format!("/api/v4/users/{me}/posts/abc/ack"),
        ),
        (
            "a user_id that is not the caller",
            format!(
                "/api/v4/users/{}/posts/{}/ack",
                fixture.reader.id, fixture.root_post
            ),
        ),
        (
            "a user_id Go's mux accepts and RequireUserId rejects",
            format!("/api/v4/users/xyz/posts/{ABSENT_ID}/ack"),
        ),
    ] {
        let (go, ours) = post_both_allowing_forward(&client, &token, &path, b"{}").await;
        assert_eq!(go.0, 501, "Go refuses before {why}");
        assert!(ours.2, "{why}: answered here");
        assert_eq!(ours.0, go.0, "{why}: status");
        assert_error_bodies_match_except_known_gaps(&go.1, &ours.1, why);
    }

    // And from a caller with no rights at all: the reader asking about the admin.
    let path = format!("/api/v4/users/{me}/posts/{}/ack", fixture.root_post);
    let (go, ours) = post_both_allowing_forward(&client, &fixture.reader.token, &path, b"{}").await;
    assert_eq!(go.0, 501, "the licence test precedes edit_other_users too");
    assert!(ours.2);
    assert_eq!(ours.0, go.0);
    assert_error_bodies_match_except_known_gaps(&go.1, &ours.1, "a caller with no rights at all");

    unwind(&client, &token, fixture).await;
}

/// `set_unread` on a DM root post: the body matches Go, and the four counters are all different.
///
/// The second half is not decoration. A body comparison against Go passes whenever the two
/// servers agree, including when they agree on four copies of the same number — which is what a
/// channel with three plain posts in it produces, and what makes "which column did you read"
/// unmutable. The fixture is built to break that tie and this asserts the tie is broken.
#[tokio::test]
async fn set_unread_on_a_dm_root_matches_go_with_four_distinct_counters() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token, "paroot").await;

    let path = format!(
        "/api/v4/users/{}/posts/{}/set_unread",
        fixture.reader.id, fixture.root_post
    );
    let (go, ours) = post_both_allowing_forward(
        &client,
        &fixture.reader.token,
        &path,
        br#"{"collapsed_threads_supported":true}"#,
    )
    .await;

    assert_eq!(go.0, 200, "Go marks the post unread");
    assert!(ours.2, "a DM root on the CRT arm is answered here");
    assert_eq!(
        (ours.0, String::from_utf8_lossy(&ours.1).into_owned()),
        (go.0, String::from_utf8_lossy(&go.1).into_owned()),
        "the ChannelUnreadAt bodies must be byte-identical, trailing newline included"
    );

    let body: serde_json::Value = serde_json::from_slice(&go.1).expect("Go's body decodes");
    let counters = [
        "msg_count",
        "msg_count_root",
        "mention_count",
        "mention_count_root",
    ]
    .map(|key| {
        body[key]
            .as_i64()
            .unwrap_or_else(|| panic!("{key} is a number"))
    });
    let distinct: std::collections::BTreeSet<i64> = counters.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        counters.len(),
        "the fixture must give the four counters four different values, or a port reading the \
         wrong column compares equal to Go anyway: {counters:?}"
    );
    assert_eq!(
        body["mention_count"].as_i64(),
        Some(4),
        "A, its two admin replies and D — every window post the reader did not write"
    );
    assert_eq!(
        body["mention_count_root"].as_i64(),
        Some(2),
        "A and D: the two replies are not roots and C is the reader's own"
    );
    assert_eq!(
        body["msg_count"].as_i64(),
        Some(7),
        "the seven posts before the window, the reader's own included"
    );
    assert_eq!(
        body["msg_count_root"].as_i64(),
        Some(5),
        "the five roots before the window"
    );
    // `LastViewedAt` is the *post's* `CreateAt - 1`, never `GetMillis()`. Asserted exactly:
    // "non-zero" is true of the stamped `LastUpdateAt` as well, and the two sit in the same
    // `SET` clause a line apart.
    let created = post_create_at(&client, &token, &fixture.root_post).await;
    assert_eq!(
        body["last_viewed_at"].as_i64(),
        Some(created - 1),
        "last_viewed_at is the marked post's create_at minus one, not the time of the request"
    );

    unwind(&client, &token, fixture).await;
}

/// The same post, two values of `collapsed_threads_supported`, two different servers answering.
///
/// A **reply** with the flag set takes the CRT arm and is served here; the same reply without it
/// takes the arm that follows the thread and recounts its mentions, which this port refuses — so
/// the request is forwarded, and Go answers it. Both bodies must still match Go's, and the
/// forwarded one must carry no `x-mmrs-served-by`.
///
/// This is the test that proves the forward is real. Without the header assertion a forwarded
/// route and a correctly-ported one are indistinguishable, because in both cases the bytes came
/// from Go.
#[tokio::test]
async fn a_reply_is_served_with_the_crt_flag_and_forwarded_without_it() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token, "pareply").await;

    let path = format!(
        "/api/v4/users/{}/posts/{}/set_unread",
        fixture.reader.id, fixture.reply_post
    );

    let (go_crt, ours_crt) = post_both_allowing_forward(
        &client,
        &fixture.reader.token,
        &path,
        br#"{"collapsed_threads_supported":true}"#,
    )
    .await;
    assert_eq!(go_crt.0, 200);
    assert!(
        ours_crt.2,
        "a reply with the flag set takes the CRT arm, which this server answers"
    );
    assert_eq!(
        (
            ours_crt.0,
            String::from_utf8_lossy(&ours_crt.1).into_owned()
        ),
        (go_crt.0, String::from_utf8_lossy(&go_crt.1).into_owned()),
    );

    let (go_plain, ours_plain) = post_both_allowing_forward(
        &client,
        &fixture.reader.token,
        &path,
        br#"{"collapsed_threads_supported":false}"#,
    )
    .await;
    assert_eq!(go_plain.0, 200);
    assert!(
        !ours_plain.2,
        "a reply without the flag needs countThreadMentions, so it must be forwarded whole"
    );
    assert_eq!(
        (
            ours_plain.0,
            String::from_utf8_lossy(&ours_plain.1).into_owned()
        ),
        (
            go_plain.0,
            String::from_utf8_lossy(&go_plain.1).into_owned()
        ),
    );

    // The two arms do not answer the same thing: the forwarded one zeroes `mention_count_root`
    // and `urgent_mention_count`, and writes `msg_count_root` as "fully caught up on roots".
    let crt: serde_json::Value = serde_json::from_slice(&go_crt.1).expect("decodes");
    let plain: serde_json::Value = serde_json::from_slice(&go_plain.1).expect("decodes");
    assert_ne!(
        crt["mention_count_root"], plain["mention_count_root"],
        "the flag changes the response shape; if these ever agree the fixture has stopped \
         discriminating and the CRT dispatch is untested"
    );
    assert_eq!(plain["mention_count_root"].as_i64(), Some(0));

    unwind(&client, &token, fixture).await;
}

/// An open channel needs the mention parser, so `set_unread` there is forwarded — and still
/// matches Go.
#[tokio::test]
async fn set_unread_in_an_open_channel_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token, "paopen").await;

    let post = post_message(&client, &token, &fixture.public_channel, "mmrs open", None).await;
    let me = logged_in_user_id();
    let path = format!("/api/v4/users/{me}/posts/{post}/set_unread");

    let (go, ours) = post_both_allowing_forward(
        &client,
        &token,
        &path,
        br#"{"collapsed_threads_supported":true}"#,
    )
    .await;
    assert_eq!(go.0, 200);
    assert!(
        !ours.2,
        "an open channel needs MentionKeywords and isPostMention, so it must be forwarded"
    );
    assert_eq!(
        (ours.0, String::from_utf8_lossy(&ours.1).into_owned()),
        (go.0, String::from_utf8_lossy(&go.1).into_owned()),
    );

    unwind(&client, &token, fixture).await;
}

/// `set_unread`'s refusals: the two permission gates and the two id validations, in Go's order.
///
/// The gates carry different permission ids, so a port that ran them in the other order — or
/// reached for `read_channel_content` where Go says `edit_other_users` — differs only in the one
/// field a client branches on.
#[tokio::test]
async fn set_unread_refusals_match_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token, "parefuse").await;
    let me = logged_in_user_id();

    let cases: &[(&str, String, &str)] = &[
        // `RequirePostId` runs before `RequireUserId`, so both-invalid names the post.
        (
            "both segments invalid names post_id",
            "/api/v4/users/abc/posts/def/set_unread".to_owned(),
            &token,
        ),
        (
            "an invalid user_id with a valid post_id",
            format!("/api/v4/users/abc/posts/{}/set_unread", fixture.root_post),
            &token,
        ),
        // The reader asking about the admin: `edit_other_users`.
        (
            "acting for another user without the permission",
            format!("/api/v4/users/{me}/posts/{}/set_unread", fixture.root_post),
            &fixture.reader.token,
        ),
        // A post that does not exist: the read-post gate cannot resolve a channel and falls back
        // to the bare system permission, which a plain user does not have.
        (
            "a post that does not exist, asked by a plain user",
            format!(
                "/api/v4/users/{}/posts/{ABSENT_ID}/set_unread",
                fixture.reader.id
            ),
            &fixture.reader.token,
        ),
    ];

    for (why, path, as_token) in cases {
        let (go, ours) = post_both_allowing_forward(&client, as_token, path, b"{}").await;
        assert!(ours.2, "{why}: refused here, not forwarded");
        assert!(
            go.0 >= 400,
            "{why} should be a refusal on Go, not a {}",
            go.0
        );
        assert_eq!(ours.0, go.0, "{why}: status");
        assert_error_bodies_match_except_known_gaps(&go.1, &ours.1, why);
    }

    unwind(&client, &token, fixture).await;
}

/// A body that is not a JSON object of booleans is **not** an error on this route.
///
/// `MapBoolFromJSON` drops the decode error and hands back an empty map, so every one of these
/// proceeds as `collapsed_threads_supported: false`. Every neighbouring route in `api4/post.go`
/// answers 400 for the same bodies, which is why this is worth pinning.
#[tokio::test]
async fn a_malformed_set_unread_body_is_not_an_error() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token, "pabody").await;

    let path = format!(
        "/api/v4/users/{}/posts/{}/set_unread",
        fixture.reader.id, fixture.root_post
    );

    for body in [
        &b""[..],
        b"not json at all",
        b"[]",
        b"\"a string\"",
        br#"{"collapsed_threads_supported":"yes"}"#,
        br#"{"collapsed_threads_supported":1}"#,
        br#"{"something_else":true}"#,
    ] {
        let (go, ours) =
            post_both_allowing_forward(&client, &fixture.reader.token, &path, body).await;
        assert_eq!(
            go.0,
            200,
            "Go accepts {:?} and treats the flag as false",
            String::from_utf8_lossy(body)
        );
        assert!(
            ours.2,
            "answered here for {:?}",
            String::from_utf8_lossy(body)
        );
        assert_eq!(
            (ours.0, String::from_utf8_lossy(&ours.1).into_owned()),
            (go.0, String::from_utf8_lossy(&go.1).into_owned()),
            "body {:?}",
            String::from_utf8_lossy(body)
        );
    }

    unwind(&client, &token, fixture).await;
}

/// An **absent** `collapsed_threads_supported` key is `false`, not `true`.
///
/// Every other test in this file sends the flag explicitly, and on a **root** post both values
/// answer the same thing — so `unwrap_or(false)` and `unwrap_or(true)` are indistinguishable
/// everywhere else in the suite. A reply with no flag at all is the one request that separates
/// them: `false` sends it to Go, `true` answers it here with a different `mention_count_root`.
///
/// The same request pins the key's spelling: read a different key and the `unwrap_or` decides,
/// which is the same observation from the other side.
#[tokio::test]
async fn an_absent_collapsed_threads_key_is_false() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token, "paabsent").await;

    let path = format!(
        "/api/v4/users/{}/posts/{}/set_unread",
        fixture.reader.id, fixture.reply_post
    );
    for body in [&b"{}"[..], b"", br#"{"collapsed_threads_supported":null}"#] {
        let (go, ours) =
            post_both_allowing_forward(&client, &fixture.reader.token, &path, body).await;
        assert_eq!(go.0, 200);
        assert!(
            !ours.2,
            "an absent flag is false, so a reply must be forwarded: body {:?}",
            String::from_utf8_lossy(body)
        );
        assert_eq!(
            (ours.0, String::from_utf8_lossy(&ours.1).into_owned()),
            (go.0, String::from_utf8_lossy(&go.1).into_owned()),
        );
    }

    unwind(&client, &token, fixture).await;
}

/// A well-formed object holding **one bad value beside a good one** keeps the good one.
///
/// `encoding/json` decoding into a `map[string]bool` treats a wrong-typed value as a `saveError`:
/// it records the `UnmarshalTypeError` and **carries on**, so every key whose value really is a
/// boolean is still written. `MapBoolFromJSON` returns that map because it is non-nil.
/// `serde_json::from_slice::<HashMap<String, bool>>` has no such notion — one bad value fails the
/// whole decode — so this shipped as `false` here and `true` on Go until it was measured.
///
/// **Asked of Go directly, not by comparing the two servers**, and that is the point. A reply
/// with the flag `false` is *forwarded*, so a body comparison passes whichever way Go decodes:
/// Go answered both halves. The first version of this test did exactly that and was green while
/// the divergence was live. The oracle is Go's own answer for an explicit `true` against an
/// explicit `false` — the mixed body must equal one of them, and which one is the finding.
#[tokio::test]
async fn a_good_flag_survives_a_bad_value_beside_it() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token, "pamixed").await;

    let path = format!(
        "{GO}/api/v4/users/{}/posts/{}/set_unread",
        fixture.reader.id, fixture.reply_post
    );
    let ask = async |body: &'static str| -> serde_json::Value {
        client
            .post(&path)
            .header("Authorization", format!("Bearer {}", fixture.reader.token))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .expect("Go answers")
            .json()
            .await
            .expect("the body decodes")
    };

    let with_true = ask(r#"{"collapsed_threads_supported":true}"#).await;
    let with_false = ask(r#"{"collapsed_threads_supported":false}"#).await;
    assert_ne!(
        with_true["mention_count_root"], with_false["mention_count_root"],
        "the two flags must answer differently for this test to mean anything"
    );

    for body in [
        r#"{"collapsed_threads_supported":true,"x":"nope"}"#,
        r#"{"x":"nope","collapsed_threads_supported":true}"#,
        r#"{"collapsed_threads_supported":true,"x":1}"#,
    ] {
        let mixed = ask(body).await;
        assert_eq!(
            mixed["mention_count_root"], with_true["mention_count_root"],
            "Go keeps the boolean key in {body} despite the bad value beside it"
        );
    }

    // And ours agrees — including on **which server answers**. The flag being `true` puts a reply
    // on the CRT arm, which this server serves, so a correct port answers here rather than
    // forwarding. That assertion is what the first version of this test was missing: it compared
    // the two bodies, we forwarded, and Go supplied both halves of a comparison that was green
    // while the divergence was live.
    let ours_path = format!(
        "/api/v4/users/{}/posts/{}/set_unread",
        fixture.reader.id, fixture.reply_post
    );
    let (go, ours) = post_both_allowing_forward(
        &client,
        &fixture.reader.token,
        &ours_path,
        br#"{"collapsed_threads_supported":true,"x":"nope"}"#,
    )
    .await;
    assert!(
        ours.2,
        "the good flag survives here too, so the reply takes the CRT arm and is answered here"
    );
    assert_eq!(
        (ours.0, String::from_utf8_lossy(&ours.1).into_owned()),
        (go.0, String::from_utf8_lossy(&go.1).into_owned()),
    );

    // The mirror: a bad value does not turn an explicit `false` into a `true`, so the reply is
    // still forwarded.
    let (_, ours_false) = post_both_allowing_forward(
        &client,
        &fixture.reader.token,
        &ours_path,
        br#"{"collapsed_threads_supported":false,"x":"nope"}"#,
    )
    .await;
    assert!(
        !ours_false.2,
        "a bad value beside an explicit false leaves it false"
    );

    unwind(&client, &token, fixture).await;
}

/// The literal `me` in `{user_id}` resolves to the session's own user.
///
/// `RequireUserId` (web/context.go:301) substitutes it, and nothing else in this file sends it —
/// so without this, dropping the substitution answers `api.context.invalid_url_param.app_error`
/// for a path every Mattermost client uses and no test notices.
#[tokio::test]
async fn me_resolves_to_the_caller_on_set_unread() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token, "pame").await;

    let by_id = format!(
        "/api/v4/users/{}/posts/{}/set_unread",
        fixture.reader.id, fixture.root_post
    );
    let by_me = format!("/api/v4/users/me/posts/{}/set_unread", fixture.root_post);
    let flag = br#"{"collapsed_threads_supported":true}"#;

    let (go_id, _) = post_both_allowing_forward(&client, &fixture.reader.token, &by_id, flag).await;
    let (go_me, ours_me) =
        post_both_allowing_forward(&client, &fixture.reader.token, &by_me, flag).await;

    assert_eq!(go_me.0, 200, "Go resolves `me`");
    assert!(ours_me.2, "`me` is answered here, not forwarded");
    assert_eq!(
        (ours_me.0, String::from_utf8_lossy(&ours_me.1).into_owned()),
        (go_me.0, String::from_utf8_lossy(&go_me.1).into_owned()),
    );
    // And `me` really is the same user: the two spellings answer the same body, `user_id`
    // included. A substitution that resolved to somebody else would still be a 200.
    assert_eq!(
        String::from_utf8_lossy(&go_id.1),
        String::from_utf8_lossy(&go_me.1),
        "`me` and the reader's own id are the same request"
    );

    unwind(&client, &token, fixture).await;
}

/// `POST …/reminder` is forwarded whole — the one route of the four this server does not answer.
///
/// Pinned so that registering it by accident, or "finishing" it without the permalink-embed
/// machinery its ephemeral confirmation needs, fails a test rather than shipping a websocket
/// event with a missing preview. See [D-420].
#[tokio::test]
async fn the_reminder_route_is_forwarded_whole() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let fixture = fixture(&client, &token, "paremind").await;

    let path = format!(
        "/api/v4/users/{}/posts/{}/reminder",
        fixture.reader.id, fixture.root_post
    );
    // Seconds, not milliseconds — `time.Unix(targetTime, 0)` at app/post.go:2866.
    let target = 4_102_444_800i64;
    let body = format!(r#"{{"target_time":{target}}}"#);
    let (go, ours) =
        post_both_allowing_forward(&client, &fixture.reader.token, &path, body.as_bytes()).await;

    assert!(
        !ours.2,
        "the reminder route must still be forwarded; its ephemeral confirmation needs the \
         permalink-embed path this port does not have"
    );
    assert_eq!(go.0, 200, "Go sets the reminder");
    assert_eq!(
        (ours.0, String::from_utf8_lossy(&ours.1).into_owned()),
        (go.0, String::from_utf8_lossy(&go.1).into_owned()),
    );

    // The row Go wrote is the one this port's store methods would have written, and the target
    // time is stored verbatim. Read back through Go's own reminder listing is not exposed over
    // REST, so this asserts only what the response says.
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&go.1)
            .ok()
            .and_then(|v| v["status"].as_str().map(str::to_owned)),
        Some("OK".to_owned()),
    );

    unwind(&client, &token, fixture).await;
}
