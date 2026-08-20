//! Cross-server parity for `GET /api/v4/users/{user_id}/channels/{channel_id}/unread`.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_channel_unread
//! ```
//!
//! # What this route adds over `getChannelMember`
//!
//! Two things, and both are the reason it is worth a session of its own:
//!
//! 1. **Two permission gates rather than one**, and the first is about the *user*
//!    (`edit_other_users`) rather than the channel. Asking about somebody else is refused at a
//!    different gate than asking about a channel one cannot read, and only a non-admin actor can
//!    tell them apart — the fixture user's `manage_system` grants both ([D-147]).
//! 2. **An app-layer transform on the body.** `mark_unread = mention` zeroes two of the seven
//!    counters. A body test on a channel with nothing in it passes either way, so every
//!    interesting case here needs somebody else to have posted first.
//!
//! # The counters are asynchronous, which shapes every fixture
//!
//! Posting kicks off unread-count work in Go, so a channel read immediately after a post is a
//! moving row. [`fetch_both_stable`] is what makes a comparison meaningful — see its docs — and
//! every body comparison below goes through it.

mod common;

use common::{
    a_team_and_channel_the_user_is_in, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel, create_plain_user,
    delete_channel, delete_plain_user, fetch_both_raw, fetch_both_stable, go_minted_token,
    logged_in_user_id, null_out_member_column, post_message, purge_api_fixtures,
    set_member_notify_prop, stack_enabled, username_of, view_channel,
};

/// Everything a body test needs: a channel the plain user is in, with unread traffic in it that
/// the plain user did not create.
///
/// Returned as `(team_id, channel_id, plain_user)`. The caller unwinds with [`unwind`].
struct Fixture {
    channel_id: String,
    user_id: String,
    token: String,
}

/// Build the fixture: a fresh channel, a fresh non-admin in it, and traffic from the admin shaped
/// so that **the four message/mention counters hold four different numbers**.
///
/// Three root posts and one threaded reply, with an `@`-mention in one root and one reply:
///
/// | counter | why it differs |
/// |---|---|
/// | `msg_count` | the posts made **after** the reader last viewed the channel |
/// | `msg_count_root` | same, minus the reply, which does not raise `TotalMsgCountRoot` |
/// | `mention_count` | both mentions |
/// | `mention_count_root` | only the mention in a root post |
///
/// That shape is the point. Three plain root posts leave `msg_count == msg_count_root` and both
/// mention counters at `0`, and a port reading the wrong column — or no column — then compares
/// equal against Go anyway. The exact values are not asserted, because Go owns them and a join
/// adds a system post; what is asserted is that they differ pairwise.
async fn fixture(client: &reqwest::Client, admin_token: &str, tag: &str) -> Fixture {
    let (team_id, _) = a_team_and_channel_the_user_is_in(client, admin_token).await;
    let channel_id = create_channel(client, admin_token, &team_id, tag).await;
    let plain = create_plain_user(client, admin_token, &team_id, tag).await;
    add_user_to_channel(client, admin_token, &channel_id, &plain.id).await;

    let username = username_of(client, admin_token, &plain.id).await;
    post_message(client, admin_token, &channel_id, "first", None).await;
    post_message(client, admin_token, &channel_id, "second", None).await;

    // **The reader catches up here**, which is what makes `MsgCount` non-zero and the store's
    // `TotalMsgCount - MsgCount` an actual subtraction. Without this the member's count stays at
    // `0`, the subtraction and a bare `SELECT TotalMsgCount` agree, and removing it is a mutation
    // that passes the whole suite — as it did, before this line.
    view_channel(client, &plain.token, &channel_id).await;

    let root = post_message(
        client,
        admin_token,
        &channel_id,
        &format!("@{username} third"),
        None,
    )
    .await;
    post_message(
        client,
        admin_token,
        &channel_id,
        &format!("@{username} fourth"),
        Some(&root),
    )
    .await;
    // A fifth post, mentioning nobody, so that `msg_count` outruns `mention_count`. Without it
    // the two happen to be equal and a swap between them is invisible — which is what the first
    // version of this fixture measured, and it is why the assertions below are pairwise.
    post_message(client, admin_token, &channel_id, "fifth", None).await;

    Fixture {
        channel_id,
        user_id: plain.id,
        token: plain.token,
    }
}

async fn unwind(client: &reqwest::Client, admin_token: &str, fixture: &Fixture) {
    delete_channel(client, admin_token, &fixture.channel_id).await;
    delete_plain_user(client, admin_token, &fixture.user_id).await;
}

/// The whole body, on a channel with real unread traffic in it, read by the member itself.
#[tokio::test]
async fn the_unread_body_is_byte_identical_across_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let f = fixture(&client, &admin_token, "unreadbody").await;

    let path = format!(
        "/api/v4/users/{}/channels/{}/unread",
        f.user_id, f.channel_id
    );
    let (go_body, rs_body) = fetch_both_stable(&client, &f.token, &path).await;
    unwind(&client, &admin_token, &f).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "the two servers must agree byte for byte"
    );

    // Not vacuous: the fixture exists so these are non-zero. A body of seven zeroes would compare
    // equal while proving nothing about the subtraction, the COALESCE or the mention columns.
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(parsed["channel_id"].as_str(), Some(f.channel_id.as_str()));
    assert!(
        parsed["msg_count"].as_i64().unwrap_or_default() > 0,
        "the admin posted three messages the plain user has not read: {parsed}"
    );
    assert!(
        parsed["mention_count"].as_i64().unwrap_or_default() > 0,
        "two of those messages @-mentioned the plain user: {parsed}"
    );

    // The four counters must hold four *different* numbers, or swapping two columns in the SELECT
    // is undetectable and this whole comparison proves less than it looks like it does. See
    // [`fixture`] for the traffic that makes them differ.
    for (a, b) in [
        ("msg_count", "msg_count_root"),
        ("mention_count", "mention_count_root"),
        ("msg_count", "mention_count"),
        ("msg_count_root", "mention_count_root"),
    ] {
        assert_ne!(
            parsed[a], parsed[b],
            "{a} and {b} must differ or a swap between them is undetectable: {parsed}"
        );
    }
    assert!(
        !parsed
            .as_object()
            .expect("an object")
            .contains_key("notify_props"),
        "`json:\"-\"` — the store loads it, the client never sees it"
    );
    assert_eq!(
        rs_body.last(),
        Some(&b'\n'),
        "this handler uses an encoder, so the body ends in a newline"
    );
}

/// `me` resolves to the session's own id **before** validation (web/context.go:301), so it must
/// produce the same bytes as the explicit id — on both servers, and on a route whose `me` sits in
/// the *first* path segment rather than the last.
#[tokio::test]
async fn the_me_alias_answers_the_same_as_the_explicit_id() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let f = fixture(&client, &admin_token, "unreadme").await;

    let explicit = format!(
        "/api/v4/users/{}/channels/{}/unread",
        f.user_id, f.channel_id
    );
    let aliased = format!("/api/v4/users/me/channels/{}/unread", f.channel_id);

    let (go_explicit, rs_explicit) = fetch_both_stable(&client, &f.token, &explicit).await;
    let (go_aliased, rs_aliased) = fetch_both_stable(&client, &f.token, &aliased).await;
    unwind(&client, &admin_token, &f).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_explicit),
        String::from_utf8_lossy(&go_explicit)
    );
    assert_eq!(
        String::from_utf8_lossy(&rs_aliased),
        String::from_utf8_lossy(&go_aliased)
    );
    assert_eq!(
        String::from_utf8_lossy(&rs_aliased),
        String::from_utf8_lossy(&rs_explicit),
        "`me` and the explicit id are the same request"
    );
}

/// **The app-layer transform, measured rather than reasoned.**
///
/// With `mark_unread = mention` the two message counts must come back `0` while the three mention
/// counts keep the values they had. The same request before the prop is set is the control: if
/// `msg_count` were zero to begin with, the "after" assertion would pass for a port that does
/// nothing at all.
#[tokio::test]
async fn mark_unread_mention_zeroes_the_message_counts_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let f = fixture(&client, &admin_token, "unreadmute").await;
    let path = format!(
        "/api/v4/users/{}/channels/{}/unread",
        f.user_id, f.channel_id
    );

    let (go_before, rs_before) = fetch_both_stable(&client, &f.token, &path).await;
    let before: serde_json::Value = serde_json::from_slice(&rs_before).expect("decodes");
    assert_eq!(
        String::from_utf8_lossy(&rs_before),
        String::from_utf8_lossy(&go_before)
    );
    assert!(
        before["msg_count"].as_i64().unwrap_or_default() > 0,
        "the control is only a control if the count starts non-zero: {before}"
    );

    set_member_notify_prop(
        &client,
        &f.token,
        &f.channel_id,
        &f.user_id,
        "mark_unread",
        "mention",
    )
    .await;

    let (go_after, rs_after) = fetch_both_stable(&client, &f.token, &path).await;
    unwind(&client, &admin_token, &f).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_after),
        String::from_utf8_lossy(&go_after),
        "the shortcut must fire identically on both servers"
    );

    let after: serde_json::Value = serde_json::from_slice(&rs_after).expect("decodes");
    assert_eq!(after["msg_count"], serde_json::json!(0));
    assert_eq!(after["msg_count_root"], serde_json::json!(0));
    assert_eq!(
        after["mention_count"], before["mention_count"],
        "mentions pierce the mute — zeroing them too would lose a notification"
    );
    assert_eq!(after["mention_count_root"], before["mention_count_root"]);
    assert_eq!(after["team_id"], before["team_id"]);
    assert_eq!(after["channel_id"], before["channel_id"]);
}

/// **The `COALESCE(UrgentMentionCount, 0)`, which nothing reachable through the REST API can
/// exercise.**
///
/// The column is nullable and Go writes `0`, so every fixture built through the API leaves it at
/// `0` — where coalescing and not coalescing give the same answer, and deleting the `COALESCE`
/// passes the whole suite. It only becomes visible against a genuine SQL NULL, so this writes one
/// straight into the shared database and then asks both servers.
///
/// Go answers `0` because its query coalesces; a port that dropped it would fail to decode a NULL
/// into `int64` and answer 500, which is exactly the shape of the divergence this pins.
#[tokio::test]
async fn a_null_urgent_mention_count_reads_as_zero_on_both_servers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let f = fixture(&client, &admin_token, "unreadnullurgent").await;

    if !null_out_member_column(&f.channel_id, &f.user_id, "urgentmentioncount").await {
        eprintln!("skipping: DATABASE_URL is unset, so the NULL cannot be written");
        unwind(&client, &admin_token, &f).await;
        return;
    }

    let path = format!(
        "/api/v4/users/{}/channels/{}/unread",
        f.user_id, f.channel_id
    );
    let (go_body, rs_body) = fetch_both_stable(&client, &f.token, &path).await;
    unwind(&client, &admin_token, &f).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body),
        "a NULL urgent count must coalesce identically"
    );
    let parsed: serde_json::Value = serde_json::from_slice(&rs_body).expect("decodes");
    assert_eq!(
        parsed["urgent_mention_count"],
        serde_json::json!(0),
        "the NULL becomes 0 in SQL, not an error and not a missing key"
    );
}

/// **An archived channel is a 404 here and a 200 from `getChannelMember`.**
///
/// `GetChannelUnread`'s query carries a bare `DeleteAt = 0`, which resolves to the *channel's*
/// column because `ChannelMembers` has none (channel_store.go:936). `GetMember` takes an
/// `includeDeleted` flag and this call site passes `true`. So the same two ids answer differently
/// on the two routes, and both answers are Go's.
///
/// Asserted together on purpose: either half alone reads as an accident of the fixture.
#[tokio::test]
async fn an_archived_channel_is_a_404_here_and_a_200_from_the_member_route() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &token).await;
    let channel_id = create_channel(&client, &token, &team_id, "unreadarchived").await;
    let me = logged_in_user_id();

    // Go's DELETE archives rather than removes: the row stays, `DeleteAt` is stamped.
    delete_channel(&client, &token, &channel_id).await;

    let unread_path = format!("/api/v4/users/{me}/channels/{channel_id}/unread");
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &token, &unread_path).await;

    assert_eq!(go_status, 404, "an archived channel has no unread state");
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &unread_path);
    assert_eq!(
        go["id"].as_str(),
        Some("app.channel.get_unread.app_error"),
        "the same id the 500 branch uses — only the status separates them"
    );

    let member_path = format!("/api/v4/channels/{channel_id}/members/{me}");
    let ((go_member_status, go_member_body), (rs_member_status, rs_member_body)) =
        fetch_both_raw(&client, &token, &member_path).await;

    assert_eq!(
        go_member_status, 200,
        "the membership survives archiving — this is the contrast"
    );
    assert_eq!(rs_member_status, go_member_status);
    assert_eq!(
        String::from_utf8_lossy(&rs_member_body),
        String::from_utf8_lossy(&go_member_body)
    );
}

/// **The two gates are distinguishable only through an actor who can be refused.**
///
/// A non-admin asking about *somebody else* fails the first gate (`edit_other_users`); the same
/// non-admin asking about *itself* in a channel it is not in fails the second (`read_channel`).
/// Both answer 403 with the same body — the permission name lives in `detailed_error`, which both
/// servers wipe — so what this pins is that the refusals *match*, while the order and the named
/// permission are pinned in-process in `channels.rs`.
#[tokio::test]
async fn both_permission_gates_refuse_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let f = fixture(&client, &admin_token, "unreadgates").await;

    // A second channel the plain user is deliberately *not* added to.
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin_token).await;
    let outsider_channel = create_channel(&client, &admin_token, &team_id, "unreadoutsider").await;

    // Gate one: the plain user asks about the admin.
    let other_user = format!(
        "/api/v4/users/{}/channels/{}/unread",
        logged_in_user_id(),
        f.channel_id
    );
    // Gate two: the plain user asks about itself, in a channel it cannot read.
    let other_channel = format!(
        "/api/v4/users/{}/channels/{outsider_channel}/unread",
        f.user_id
    );

    for path in [&other_user, &other_channel] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &f.token, path).await;

        assert_eq!(go_status, 403, "{path}");
        assert_eq!(rs_status, go_status, "{path}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
        assert_eq!(
            go["id"].as_str(),
            Some("api.context.permissions.app_error"),
            "{path}"
        );
    }

    // And the grant, so the two refusals above are not simply "this user is refused everything".
    let own = format!(
        "/api/v4/users/{}/channels/{}/unread",
        f.user_id, f.channel_id
    );
    let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &f.token, &own).await;
    assert_eq!(
        go_status, 200,
        "the plain user may read its own unread state"
    );
    assert_eq!(rs_status, go_status);

    delete_channel(&client, &admin_token, &outsider_channel).await;
    unwind(&client, &admin_token, &f).await;
}

/// A well-formed id that names nothing. The permission check misses first and denies, so this is
/// a **403** and not a 404 — the same inference-blocking shape `getChannelMember` has.
#[tokio::test]
async fn a_missing_channel_refuses_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = format!(
        "/api/v4/users/{}/channels/aaaaaaaaaaaaaaaaaaaaaaaaaa/unread",
        logged_in_user_id()
    );

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(&client, &token, &path).await;

    assert_eq!(
        rs_status, go_status,
        "the two servers must agree on refusal"
    );
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
    assert_eq!(
        go["status_code"].as_i64(),
        Some(i64::from(go_status)),
        "the body's copy of the status is what a client reads"
    );
}

/// Malformed but **alphanumeric** ids reach the handler on both servers and 400 there. The
/// channel is validated first, so a request with both segments malformed reports `channel_id` —
/// even though the user id is the earlier path segment.
#[tokio::test]
async fn malformed_ids_refuse_identically() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    for (path, expected_param) in [
        (
            format!("/api/v4/users/{}/channels/nope/unread", logged_in_user_id()),
            "channel_id",
        ),
        (
            "/api/v4/users/nope/channels/aaaaaaaaaaaaaaaaaaaaaaaaaa/unread".to_owned(),
            "user_id",
        ),
        // Both malformed: Go reports the **channel**, because `RequireChannelId()` is chained
        // first — the opposite of the order the path spells them in.
        (
            "/api/v4/users/nope/channels/alsonope/unread".to_owned(),
            "channel_id",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, &path).await;

        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, go_status, "{path}");

        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path);
        assert_eq!(
            go["id"].as_str(),
            Some("api.context.invalid_url_param.app_error"),
            "{path}"
        );
        assert!(
            go["message"]
                .as_str()
                .unwrap_or_default()
                .contains(expected_param),
            "{path}: Go's translated message names the offending parameter, and it should be \
             {expected_param}: {}",
            go["message"]
        );
    }
}

/// **The mux charset, which is a routing rule and not a validation rule — [D-150].**
///
/// Go registers these segments as `{channel_id:[A-Za-z0-9]+}` (api4/api.go:203, :223), so a
/// segment containing anything else never matches the route: gorilla/mux answers its own 404 with
/// `api.context.404.app_error`, before any handler runs. axum's `{name}` matches the whole
/// segment, so the first version of this port reached the handler and answered **400
/// `invalid_url_param`** — a different status, id and body on a request Go never routed.
///
/// Closed by forwarding such requests instead of handling them, so the answer is literally Go's.
/// The two cases below cover both id-shaped segments; `no.pe` is there because `.` is the
/// character a client is most likely to send by accident.
#[tokio::test]
async fn a_non_alphanumeric_segment_answers_exactly_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    for path in [
        format!(
            "/api/v4/users/{}/channels/no-pe/unread",
            logged_in_user_id()
        ),
        "/api/v4/users/no.pe/channels/aaaaaaaaaaaaaaaaaaaaaaaaaa/unread".to_owned(),
        // The member route shares the fix, and shares the bug without it.
        format!("/api/v4/channels/no-pe/members/{}", logged_in_user_id()),
    ] {
        let response = client
            .get(format!("http://127.0.0.1:8066{path}"))
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
            "{path}: Go would not have routed this, so we must not handle it"
        );

        let status = response.status().as_u16();
        let body = response.bytes().await.expect("body reads").to_vec();
        assert_eq!(status, 404, "{path}");

        let rs: serde_json::Value = serde_json::from_slice(&body).expect("Go's 404 is an AppError");
        assert_eq!(
            rs["id"].as_str(),
            Some("api.context.404.app_error"),
            "{path}: the mux NotFoundHandler's id, not the handler's invalid_url_param"
        );
        // Go's 404 handler never sets a request id, and `AppError` omits an empty one — so the
        // key is absent rather than present-and-blank. Forwarding preserves that; a reproduction
        // would have had to remember it.
        assert!(
            rs.get("request_id").is_none(),
            "{path}: Handle404 sets no request id: {rs}"
        );

        // And the same request against Go directly, byte for byte apart from the URL it echoes.
        let direct = client
            .get(format!("http://localhost:8065{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(direct.status().as_u16(), status, "{path}");
        assert_eq!(
            String::from_utf8_lossy(&direct.bytes().await.expect("body reads")),
            String::from_utf8_lossy(&body),
            "{path}"
        );
    }
}

/// Everything except GET on this path still reaches Go. `partially_migrated` is what makes that
/// true, and a route registered without it would answer 405 for methods Go implements.
#[tokio::test]
async fn other_methods_on_this_path_are_still_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = format!(
        "/api/v4/users/{}/channels/aaaaaaaaaaaaaaaaaaaaaaaaaa/unread",
        logged_in_user_id()
    );

    let response = client
        .post(format!("http://127.0.0.1:8066{path}"))
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
}
