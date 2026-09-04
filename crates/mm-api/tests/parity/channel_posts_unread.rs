//! Cross-server parity for
//! `GET /api/v4/users/{user_id}/channels/{channel_id}/posts/unread`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh -p mm-api --test parity channel_posts_unread
//! ```
//!
//! # There are two completely different responses behind one route
//!
//! With something unread, the answer is a window built around the **first unread post** — the
//! thread it belongs to, then `limit_before` older posts, then `limit_after - 1` newer ones —
//! and it carries **no `ETag` at all**. With nothing unread (or a channel never opened), the
//! around-query returns an empty `order`, Go throws it away and re-fetches a plain first page,
//! and *that* branch is the only one that computes an etag. So whether this route can answer 304
//! depends on whether the caller is caught up.
//!
//! # The fixture has to control `LastViewedAt`, which means it has to control time
//!
//! `POST /channels/members/me/view` stamps the member's `LastViewedAt` with the current
//! millisecond, and the cursor is a strict `CreateAt > LastViewedAt`. A post written in that same
//! millisecond is therefore *not* unread. The fixture separates the two with a short sleep — the
//! one place in this suite where waiting is the point rather than a smell.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_channel_typed, create_plain_user, fetch_both, fetch_both_raw,
    go_minted_token, logged_in_user_id, post_message, purge_api_fixtures, stack_enabled,
    view_channel,
};

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

struct Fixture {
    /// Read up to `read_c`, then four more posts written — one of them a reply.
    channel_id: String,
    read_a: String,
    read_b: String,
    /// A reply **in the read half** of the channel. Without it the collapsed *before*-window has
    /// no reply to exclude and its `RootId = ''` predicate is untestable — a mutation dropping
    /// it survived until this post existed.
    read_b_reply: String,
    read_c: String,
    /// The first unread post, and the one the window is built around.
    unread_d: String,
    /// A reply to `unread_d`, so the thread half of the response has something in it.
    unread_d_reply: String,
    unread_e: String,
    unread_f: String,
    /// A reply to `read_a` written **after** everything else, so its root is deep in the
    /// before-window while the reply itself sits past the end of a small after-window. That is
    /// the only shape that makes the parents pass observable: with `skipFetchThreads` off it
    /// pulls this post into `posts` without putting it in `order`.
    late_reply_to_read_a: String,
    /// Joined and never viewed: `LastViewedAt == 0`, the first empty-list branch.
    never_viewed_id: String,
    /// Viewed after the last post: nothing newer, the second empty-list branch.
    caught_up_id: String,
    /// Viewed, and then the column set to SQL NULL. `COALESCE(LastViewedAt, 0)` makes that the
    /// same as never viewed; nothing in the REST API can write the NULL, so the fixture does.
    null_viewed_id: String,
    /// Never viewed, and holding more than `web.LimitMaximum` posts — the only way to see the
    /// `limit_before` clamp, which is invisible in a channel smaller than the cap.
    bulk_channel_id: String,
    /// A private channel the reader is not in.
    private_channel_id: String,
    /// The non-admin whose unread state all of the above describes.
    reader_id: String,
    reader_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

/// Long enough that the millisecond clock has certainly moved on. `LastViewedAt` is stamped in
/// milliseconds and the cursor is strict, so a post written inside the same millisecond as the
/// view would be read rather than unread and the whole fixture would describe a different case.
async fn past_the_millisecond() {
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let (team_id, _) = common::a_team_and_channel_the_user_is_in(client, token).await;

            let channel_id = create_channel(client, token, &team_id, "unreadmain").await;
            let reader = create_plain_user(client, token, &team_id, "unread").await;
            add_user_to_channel(client, token, &channel_id, &reader.id).await;

            // Three posts, then the reader catches up, then four more. The admin writes them all
            // so that viewing does not also move the counters being measured.
            let read_a = post_message(client, token, &channel_id, "read one", None).await;
            let read_b = post_message(client, token, &channel_id, "read two", None).await;
            let read_b_reply = post_message(
                client,
                token,
                &channel_id,
                "a reply in the read half",
                Some(&read_b),
            )
            .await;
            let read_c = post_message(client, token, &channel_id, "read three", None).await;

            past_the_millisecond().await;
            view_channel(client, &reader.token, &channel_id).await;
            past_the_millisecond().await;

            let unread_d = post_message(client, token, &channel_id, "unread one", None).await;
            let unread_d_reply = post_message(
                client,
                token,
                &channel_id,
                "a reply to the first unread",
                Some(&unread_d),
            )
            .await;
            let unread_e = post_message(client, token, &channel_id, "unread two", None).await;
            let unread_f = post_message(client, token, &channel_id, "unread three", None).await;
            let late_reply_to_read_a = post_message(
                client,
                token,
                &channel_id,
                "a late reply to the very first post",
                Some(&read_a),
            )
            .await;

            let never_viewed_id = create_channel(client, token, &team_id, "unreadnever").await;
            add_user_to_channel(client, token, &never_viewed_id, &reader.id).await;
            post_message(client, token, &never_viewed_id, "never seen", None).await;

            let caught_up_id = create_channel(client, token, &team_id, "unreadcaught").await;
            add_user_to_channel(client, token, &caught_up_id, &reader.id).await;
            post_message(client, token, &caught_up_id, "all read", None).await;
            past_the_millisecond().await;
            view_channel(client, &reader.token, &caught_up_id).await;

            let null_viewed_id = create_channel(client, token, &team_id, "unreadnull").await;
            add_user_to_channel(client, token, &null_viewed_id, &reader.id).await;
            post_message(client, token, &null_viewed_id, "one post", None).await;
            past_the_millisecond().await;
            view_channel(client, &reader.token, &null_viewed_id).await;
            common::null_out_member_column(&null_viewed_id, &reader.id, "lastviewedat").await;

            // 210 posts, comfortably past `web.LimitMaximum` (200). Sequential because the order
            // does not matter here — only the count does.
            let bulk_channel_id = create_channel(client, token, &team_id, "unreadbulk").await;
            add_user_to_channel(client, token, &bulk_channel_id, &reader.id).await;
            for i in 0..210 {
                post_message(client, token, &bulk_channel_id, &format!("bulk {i}"), None).await;
            }

            let private_channel_id =
                create_channel_typed(client, token, &team_id, "unreadpriv", "P").await;
            add_user_to_channel(client, token, &private_channel_id, logged_in_user_id()).await;
            post_message(client, token, &private_channel_id, "members only", None).await;

            Fixture {
                channel_id,
                read_a,
                read_b,
                read_b_reply,
                read_c,
                unread_d,
                unread_d_reply,
                unread_e,
                unread_f,
                late_reply_to_read_a,
                never_viewed_id,
                caught_up_id,
                null_viewed_id,
                bulk_channel_id,
                private_channel_id,
                reader_id: reader.id,
                reader_token: reader.token,
            }
        })
        .await
}

fn order_of(body: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .expect("JSON")
        .get("order")
        .expect("an order")
        .as_array()
        .expect("an array")
        .iter()
        .map(|v| v.as_str().expect("an id").to_owned())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// the unread window
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_unread_window_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/users/me/channels/{}/posts/unread", f.channel_id);
    let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path} must be byte-identical"
    );
    assert!(
        go.ends_with(b"\n"),
        "PostList.EncodeJSON appends the newline json.Encoder writes"
    );

    let order = order_of(&go);
    // The default limits are 60 either way, so the whole channel fits. Filtered to the fixture's
    // own posts, because Go writes **system posts** of its own — one when the channel is created
    // and one for each member added — and they sit in the history alongside the seven this
    // fixture authored. Asserting a bare length here would be asserting how many join messages
    // Mattermost happens to write.
    let mine: Vec<String> = order
        .iter()
        .filter(|id| {
            [
                &f.late_reply_to_read_a,
                &f.unread_f,
                &f.unread_e,
                &f.unread_d_reply,
                &f.unread_d,
                &f.read_c,
                &f.read_b_reply,
                &f.read_b,
                &f.read_a,
            ]
            .contains(id)
        })
        .cloned()
        .collect();
    assert_eq!(
        mine,
        vec![
            f.late_reply_to_read_a.clone(),
            f.unread_f.clone(),
            f.unread_e.clone(),
            f.unread_d_reply.clone(),
            f.unread_d.clone(),
            f.read_c.clone(),
            f.read_b_reply.clone(),
            f.read_b.clone(),
            f.read_a.clone(),
        ],
        "SortByCreateAt leaves the list newest-first"
    );
    assert_eq!(
        order.first(),
        Some(&f.late_reply_to_read_a),
        "and the newest post overall is the last one written"
    );
    assert!(
        order.len() > mine.len(),
        "the system posts really are in there, so the filter above is not hiding an empty list"
    );
}

/// The route's whole point: the window is built around the **first unread** post, so shrinking
/// `limit_before` cuts history and leaves everything from the cursor forward intact.
#[tokio::test]
async fn limit_before_trims_history_around_the_unread_post() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!(
        "/api/v4/users/me/channels/{}/posts/unread?limit_before=1&limit_after=60",
        f.channel_id
    );
    let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));

    let order = order_of(&go);
    assert_eq!(
        order,
        vec![
            f.late_reply_to_read_a.clone(),
            f.unread_f.clone(),
            f.unread_e.clone(),
            f.unread_d_reply.clone(),
            f.unread_d.clone(),
            f.read_c.clone(),
        ],
        "one post of history, and everything from the cursor forward"
    );

    // `limit_before=0` is legal and asks for none at all — unlike `limit_after=0`, which 400s.
    let none = format!(
        "/api/v4/users/me/channels/{}/posts/unread?limit_before=0&limit_after=60",
        f.channel_id
    );
    let (go, rs) = fetch_both(&client, &f.reader_token, &none).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
    let order = order_of(&go);
    assert!(
        !order.contains(&f.read_c),
        "no history was asked for: {order:?}"
    );
    assert!(order.contains(&f.unread_d), "the cursor itself stays");
}

/// `limitAfter - 1`, because the unread post itself already occupies a slot. With
/// `limit_after=2` exactly one newer post joins it.
#[tokio::test]
async fn limit_after_counts_the_unread_post_itself() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!(
        "/api/v4/users/me/channels/{}/posts/unread?limit_before=0&limit_after=2",
        f.channel_id
    );
    let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));

    let order = order_of(&go);
    assert_eq!(
        order.len(),
        2,
        "the cursor plus limit_after - 1 = one more: {order:?}"
    );
    assert!(order.contains(&f.unread_d));
    assert!(order.contains(&f.unread_d_reply));
    assert!(!order.contains(&f.unread_e));
}

/// `reply_count` on this route survives a round trip through code that sets it to zero, and the
/// only reason it does is the parents pass.
///
/// `getPostsAround` scans into `postWithExtra`; the non-collapsed query aliases its subquery
/// `ReplyCount`, which lands on the embedded `Post.ReplyCount`, and then `processPost` runs
/// `p.Post.ReplyCount = p.ThreadReplyCount` unconditionally (post_store.go:1288) — with
/// `ThreadReplyCount` selected only on the *collapsed* branch, so on this one it is zero. Every
/// window post therefore leaves `prepareThreadedResponse` reporting zero replies.
///
/// It is then **immediately undone**: the parents query re-fetches every window post (its
/// `rootIds` list is built from each post's own id) into a plain `[]*model.Post` that
/// `processPost` never touches, and `AddPost` overwrites the map entry. So the zeroing is
/// unobservable through this route, and a client sees real counts.
///
/// Asserted rather than reasoned about: the first version of this test predicted zeroes, and Go
/// answered `1`.
#[tokio::test]
async fn the_parents_pass_restores_the_reply_count_processpost_zeroed() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/users/me/channels/{}/posts/unread", f.channel_id);
    let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed["posts"][&f.unread_d]["reply_count"], 1,
        "the cursor post's thread has one reply"
    );
    assert_eq!(
        parsed["posts"][&f.read_c]["reply_count"], 0,
        "…and a root with no replies still reports zero, so the assertion above is about the \
         count and not about the field being present. Not `read_a`: the parents-pass fixture \
         gave that one a late reply."
    );

    // `skipFetchThreads` narrows the parents pass to the window's own posts and their roots. The
    // count survives that too, because a window post is always in its own `rootIds`.
    let skipping = format!(
        "/api/v4/users/me/channels/{}/posts/unread?skipFetchThreads=true",
        f.channel_id
    );
    let (go, rs) = fetch_both(&client, &f.reader_token, &skipping).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(parsed["posts"][&f.unread_d]["reply_count"], 1);

    // The collapsed branch has no parents pass at all — its count comes from the `Threads` table
    // through `ThreadReplyCount`, which is the one place `processPost`'s assignment is the point.
    let collapsed = format!(
        "/api/v4/users/me/channels/{}/posts/unread?collapsedThreads=true",
        f.channel_id
    );
    let (go, rs) = fetch_both(&client, &f.reader_token, &collapsed).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert_eq!(
        parsed["posts"][&f.unread_d]["reply_count"], 1,
        "the Threads row carries the same count by a completely different route"
    );
}

/// The parents pass: with `skipFetchThreads` **off**, every reply of every post in the window is
/// added to `posts` — without being added to `order`.
///
/// `late_reply_to_read_a` is the only post in the fixture that can show this. Its root is deep in
/// the before-window and the reply itself is newer than a two-post after-window, so it is outside
/// the window in both directions and can only arrive through `RootId IN (…)`. Every other reply
/// here is in the window on its own account, which is why a mutation inverting that predicate
/// survived until this post existed.
#[tokio::test]
async fn the_parents_pass_adds_replies_to_posts_without_adding_them_to_order() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!(
        "/api/v4/users/me/channels/{}/posts/unread?limit_before=60&limit_after=2",
        f.channel_id
    );
    let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );

    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    let order = order_of(&go);
    assert!(
        !order.contains(&f.late_reply_to_read_a),
        "the after-window holds two posts and this is not one of them: {order:?}"
    );
    assert!(
        parsed["posts"][&f.late_reply_to_read_a].is_object(),
        "…and yet it is in `posts`, because its root is in the window"
    );

    // `skipFetchThreads=true` narrows the parents pass to the window's own posts and their roots,
    // so the same request loses it. Without this half the assertion above would hold for a port
    // that ignored the flag entirely.
    let skipping = format!(
        "/api/v4/users/me/channels/{}/posts/unread?limit_before=60&limit_after=2&skipFetchThreads=true",
        f.channel_id
    );
    let (go, rs) = fetch_both(&client, &f.reader_token, &skipping).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{skipping}"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&go).expect("JSON");
    assert!(
        parsed["posts"][&f.late_reply_to_read_a].is_null(),
        "skipFetchThreads drops the sibling replies the parents pass would have added"
    );
}

/// No etag when there is something unread, so `If-None-Match` cannot produce a 304 here.
#[tokio::test]
async fn the_unread_window_carries_no_etag() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/users/me/channels/{}/posts/unread", f.channel_id);
    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {}", f.reader_token))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status(), 200, "{base}");
        assert!(
            response.headers().get("ETag").is_none(),
            "{base}: the etag belongs to the empty-list fallback only"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// the empty-list fallback
// ---------------------------------------------------------------------------------------------

/// Both ways of having nothing unread fall back to a plain first page — **and** that page is the
/// only branch with an etag, which then round-trips into a 304.
#[tokio::test]
async fn nothing_unread_falls_back_to_a_page_with_an_etag() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    for (channel, why) in [
        (&f.never_viewed_id, "LastViewedAt == 0"),
        (&f.caught_up_id, "no post newer than LastViewedAt"),
    ] {
        let path = format!("/api/v4/users/me/channels/{channel}/posts/unread");
        let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{why}: {path}"
        );

        // The fallback served the channel's posts rather than an empty list. Not a length: Go's
        // own system posts share the channel — see `the_unread_window_is_byte_identical`.
        let order = order_of(&go);
        assert!(!order.is_empty(), "{why}: a page, not an empty list");

        let mut etags = Vec::new();
        for base in [GO, RUST] {
            let response = client
                .get(format!("{base}{path}"))
                .header("Authorization", format!("Bearer {}", f.reader_token))
                .send()
                .await
                .expect("reachable");
            etags.push(
                response
                    .headers()
                    .get("ETag")
                    .unwrap_or_else(|| panic!("{base} sets an ETag on the fallback branch"))
                    .to_str()
                    .expect("ASCII")
                    .to_owned(),
            );
        }
        assert_eq!(etags[0], etags[1], "{why}: the two etags must agree");

        for base in [GO, RUST] {
            let response = client
                .get(format!("{base}{path}"))
                .header("Authorization", format!("Bearer {}", f.reader_token))
                .header("If-None-Match", &etags[0])
                .send()
                .await
                .expect("reachable");
            assert_eq!(response.status(), 304, "{why}: {base} honours the etag");
        }
    }
}

/// The fallback's page size is `limit_before`, not `limit_after` and not the 60-per-page default.
#[tokio::test]
async fn the_fallback_page_is_sized_by_limit_before() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // A never-viewed channel with three posts, so a page of one is distinguishable.
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel = create_channel(&client, &token, &team_id, "unreadpage").await;
    add_user_to_channel(&client, &token, &channel, &f.reader_id).await;
    for message in ["one", "two", "three"] {
        post_message(&client, &token, &channel, message, None).await;
    }

    let path =
        format!("/api/v4/users/me/channels/{channel}/posts/unread?limit_before=1&limit_after=60");
    let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
    assert_eq!(
        order_of(&go).len(),
        1,
        "PerPage is limitBefore, so one post came back out of three"
    );
}

// ---------------------------------------------------------------------------------------------
// parameters, gates and routing
// ---------------------------------------------------------------------------------------------

/// `LastViewedAt` is a nullable column and `GetMemberLastViewedAt` `COALESCE`s it to zero, which
/// the app layer reads as "never opened" — so a NULL takes the fallback branch, etag and all.
/// Nothing in the REST API can write that NULL; the fixture does it directly.
#[tokio::test]
async fn a_null_last_viewed_at_is_the_same_as_never_viewed() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!(
        "/api/v4/users/me/channels/{}/posts/unread",
        f.null_viewed_id
    );
    let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );

    // The fallback branch is the observable difference: it is the only one that sets an ETag.
    for base in [GO, RUST] {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {}", f.reader_token))
            .send()
            .await
            .expect("reachable");
        assert_eq!(response.status(), 200, "{base}");
        assert!(
            response.headers().get("ETag").is_some(),
            "{base}: a NULL LastViewedAt coalesces to 0 and takes the fallback"
        );
    }
}

/// `limit_before` is clamped to `web.LimitMaximum` (200), which is invisible in a channel with
/// fewer posts than that — so this one has 210.
#[tokio::test]
async fn limit_before_is_clamped_to_two_hundred() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // Never viewed, so the fallback runs and its page size is `limit_before`.
    let path = format!(
        "/api/v4/users/me/channels/{}/posts/unread?limit_before=99999&limit_after=60",
        f.bulk_channel_id
    );
    let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{path}"
    );
    assert_eq!(
        order_of(&go).len(),
        200,
        "asking for 99999 gets LimitMaximum, and the channel has more than that"
    );
}

/// The one pagination value on this route that is a 400 rather than a clamp.
#[tokio::test]
async fn limit_after_zero_is_a_400_and_everything_else_clamps() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let bad = format!(
        "/api/v4/users/me/channels/{}/posts/unread?limit_after=0",
        f.channel_id
    );
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.reader_token, &bad).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &bad);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");

    // Negative, garbage and over-maximum all fall to a clamp and answer 200 identically — so the
    // 400 above really is specific to the literal zero.
    for query in [
        "limit_after=-1",
        "limit_after=abc",
        "limit_after=99999",
        "limit_before=-1",
        "limit_before=abc",
        "limit_before=99999",
    ] {
        let path = format!(
            "/api/v4/users/me/channels/{}/posts/unread?{query}",
            f.channel_id
        );
        let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
    }
}

/// `r.URL.Query().Get(...) == "true"` is an exact compare, so `collapsedThreads=1` is **false**
/// here where a route using `strconv.ParseBool` would read it as true.
#[tokio::test]
async fn the_boolean_flags_compare_against_the_literal_true() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let base_path = format!("/api/v4/users/me/channels/{}/posts/unread", f.channel_id);
    let (plain, _) = fetch_both(&client, &f.reader_token, &base_path).await;

    for query in [
        "collapsedThreads=1",
        "collapsedThreads=TRUE",
        "skipFetchThreads=1",
    ] {
        let path = format!("{base_path}?{query}");
        let (go, rs) = fetch_both(&client, &f.reader_token, &path).await;
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{path}"
        );
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&plain),
            "{path}: anything but the literal `true` is the default request"
        );
    }

    // And the literal `true` really does change the answer, or the assertion above is vacuous.
    let collapsed = format!("{base_path}?collapsedThreads=true");
    let (go, rs) = fetch_both(&client, &f.reader_token, &collapsed).await;
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
    assert_ne!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&plain),
        "collapsed threads filters the reply out of `order`"
    );
    let collapsed_order = order_of(&go);
    assert!(
        !collapsed_order.contains(&f.unread_d_reply),
        "the collapsed *after*-window is `RootId = ''` only"
    );
    assert!(
        !collapsed_order.contains(&f.read_b_reply),
        "…and so is the collapsed *before*-window, which is a separate statement with its own \
         copy of the predicate"
    );
    assert!(
        !collapsed_order.contains(&f.late_reply_to_read_a),
        "…and the late reply, which is in the after-window"
    );
    assert!(
        collapsed_order.contains(&f.read_b),
        "the reply's root is still there, so the exclusion above is about replies and not \
         about the whole window being empty"
    );
}

/// `collapsedThreadsExtended=true` is forwarded, for the reason `getPostsForChannel` gives.
#[tokio::test]
async fn the_extended_variant_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!(
        "/api/v4/users/me/channels/{}/posts/unread?collapsedThreads=true&collapsedThreadsExtended=true",
        f.channel_id
    );
    let response = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {}", f.reader_token))
        .send()
        .await
        .expect("reachable");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "extended participants need SanitizeProfile"
    );
}

/// Asking about somebody else needs `edit_other_users`, and `me` resolves to the session.
#[tokio::test]
async fn asking_about_another_user_needs_edit_other_users() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    // The reader asking about the admin: refused at the first gate.
    let theirs = format!(
        "/api/v4/users/{}/channels/{}/posts/unread",
        logged_in_user_id(),
        f.channel_id
    );
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.reader_token, &theirs).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &theirs);

    // The admin asking about the reader: allowed, and the same answer `me` gives the reader.
    let explicit = format!(
        "/api/v4/users/{}/channels/{}/posts/unread",
        f.reader_id, f.channel_id
    );
    let (go, rs) = fetch_both(&client, &token, &explicit).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{explicit}"
    );
    assert_eq!(
        order_of(&go),
        order_of(
            &fetch_both(
                &client,
                &f.reader_token,
                &format!("/api/v4/users/me/channels/{}/posts/unread", f.channel_id)
            )
            .await
            .0
        ),
        "the admin sees the reader's unread state, not its own"
    );
}

/// The channel gate is `read_channel_content`, and it runs after the `GetChannel` that can 404.
#[tokio::test]
async fn an_outsider_is_refused_and_an_unknown_channel_is_a_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let private = format!(
        "/api/v4/users/me/channels/{}/posts/unread",
        f.private_channel_id
    );
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &f.reader_token, &private).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &private);

    let unknown = "/api/v4/users/me/channels/zzzzzzzzzzzzzzzzzzzzzzzzzz/posts/unread";
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, unknown).await;
    assert_eq!(go_status, 404, "GetChannel runs before the channel gate");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, unknown);
}

/// A caller who is not a member of the channel at all: `GetMemberLastViewedAt` misses and the
/// app layer raises the missing-member 404. Reachable only for someone the *channel* gate lets
/// through without a membership — a system admin on a public channel.
#[tokio::test]
async fn a_non_member_who_passes_the_channel_gate_gets_the_missing_member_404() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    // The fixture is not read here, but building it first keeps this test's own channel out of
    // the window `purge_api_fixtures` clears.
    let _ = fixture(&client, &token).await;

    // A public channel the admin created but was then removed from — public, so
    // `read_public_channel` still admits it, while `ChannelMembers` has no row.
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel = create_channel(&client, &token, &team_id, "unreadnomember").await;
    post_message(&client, &token, &channel, "a post", None).await;
    let left = client
        .delete(format!(
            "{GO}/api/v4/channels/{channel}/members/{}",
            logged_in_user_id()
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(left.status().is_success(), "the admin leaves the channel");

    let path = format!("/api/v4/users/me/channels/{channel}/posts/unread");
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 404);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(go["id"], "app.channel.get_member.missing.app_error");
}

/// `RequireUserId` runs **first** here, the reverse of `getChannelUnread` one segment up. With
/// both ids malformed the two routes name different parameters.
#[tokio::test]
async fn the_user_id_is_validated_before_the_channel_id() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let _ = fixture(&client, &token).await;

    let both_bad = "/api/v4/users/short/channels/alsoshort/posts/unread";
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, both_bad).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, both_bad);
    assert_eq!(go["id"], "api.context.invalid_url_param.app_error");

    // The sibling route on the same two malformed segments — Go validates the *channel* first
    // there. Both bodies are the same shape, so the difference is invisible over HTTP and this
    // is asserted as agreement rather than as a distinction.
    let sibling = "/api/v4/users/short/channels/alsoshort/unread";
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, sibling).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, sibling);
}

/// The three writes under `ChannelForUser` and every non-GET method on this path stay Go's.
#[tokio::test]
async fn the_neighbouring_routes_are_still_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let path = format!("/api/v4/users/me/channels/{}/posts/unread", f.channel_id);
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

    for suffix in ["posts", "posts/unread/", "notify_props"] {
        let path = format!("/api/v4/users/me/channels/{}/{suffix}", f.channel_id);
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
}
