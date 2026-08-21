//! Cross-server parity for `GET /api/v4/users/{user_id}/teams/{team_id}/unread` (`getTeamUnread`).
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_team_unread
//! ```
//!
//! # Why this one *can* compare bytes and its plural sibling cannot
//!
//! `getTeamsUnreadForUser` assembles a slice out of a Go map and is order-random per request, so
//! `parity_teams_unread.rs` compares sorted `Value`s. This route answers a single struct, and
//! `encoding/json` writes struct fields in declaration order — so every assertion here is on raw
//! bytes, trailing newline included. The newline is the point: the singular handler uses
//! `json.NewEncoder(w).Encode` (team.go:1341) where the plural uses `json.Marshal` + `w.Write`
//! (team.go:788) — two handlers for the same struct in one file with opposite answers.
//!
//! Every fixture is created and unwound here; [`common::purge_api_fixtures`] clears what a
//! panicking run leaves behind.

mod common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_channel, create_plain_user, delete_channel, delete_plain_user, fetch_both_raw,
    fetch_both_stable, go_minted_token, logged_in_user_id, post_message, purge_api_fixtures,
    set_member_notify_prop, stack_enabled, username_of, view_channel,
};

/// Go's own per-channel answer, `GET /users/{id}/channels/{id}/unread` — one term of the sum.
async fn go_channel_unread(
    client: &reqwest::Client,
    token: &str,
    user_id: &str,
    channel_id: &str,
) -> serde_json::Value {
    client
        .get(format!(
            "{GO}/api/v4/users/{user_id}/channels/{channel_id}/unread"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the unread decodes")
}

/// Create a team through Go's API and return its id. The `mmrs-parity-` prefix is what
/// [`common::purge_api_fixtures`] clears, and the team is deliberately **not** archived at the
/// end of a test: other tests in this binary run concurrently and pick a team out of the admin's
/// list, and archiving one mid-run archives its channels under them.
async fn create_team(client: &reqwest::Client, admin_token: &str, tag: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/teams"))
        .header("Authorization", format!("Bearer {admin_token}"))
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
        "creating the team failed: {}",
        response.text().await.unwrap_or_default()
    );
    let team: serde_json::Value = response.json().await.expect("the team decodes");
    team["id"].as_str().expect("an id").to_owned()
}

struct Fixture {
    team_id: String,
    loud: String,
    quiet: String,
    user_id: String,
    token: String,
}

/// A fresh non-admin on a **fresh team of its own**, in two fresh channels, with traffic shaped
/// so the team totals are sums the fold has to get right.
///
/// The team is private to the fixture because this route sums over *every* channel of a team the
/// caller is in, `town-square` included — and adding a user to a team posts a join message there.
/// Two tests sharing a team therefore move each other's totals under `fetch_both_stable`, which a
/// no-op mutation control caught once. Each fixture owns its team.
///
/// - `loud`: two root posts, the reader catches up (so `TotalMsgCount - MsgCount` is a real
///   subtraction rather than a copy), then a mentioning root, a mentioning reply and a plain root.
/// - `quiet`: one mentioning root and one plain reply — a second channel with its own distinct
///   counters, so a fold that stops after the first row lands on a different sum than Go.
async fn fixture(client: &reqwest::Client, admin_token: &str, tag: &str) -> Fixture {
    let team_id = create_team(client, admin_token, tag).await;
    let loud = create_channel(client, admin_token, &team_id, &format!("{tag}loud")).await;
    let quiet = create_channel(client, admin_token, &team_id, &format!("{tag}quiet")).await;
    let plain = create_plain_user(client, admin_token, &team_id, tag).await;
    add_user_to_channel(client, admin_token, &loud, &plain.id).await;
    add_user_to_channel(client, admin_token, &quiet, &plain.id).await;
    let username = username_of(client, admin_token, &plain.id).await;

    post_message(client, admin_token, &loud, "first", None).await;
    post_message(client, admin_token, &loud, "second", None).await;
    view_channel(client, &plain.token, &loud).await;
    let root = post_message(
        client,
        admin_token,
        &loud,
        &format!("@{username} third"),
        None,
    )
    .await;
    post_message(
        client,
        admin_token,
        &loud,
        &format!("@{username} fourth"),
        Some(&root),
    )
    .await;
    post_message(client, admin_token, &loud, "fifth", None).await;

    let quiet_root = post_message(
        client,
        admin_token,
        &quiet,
        &format!("@{username} hello"),
        None,
    )
    .await;
    post_message(client, admin_token, &quiet, "reply", Some(&quiet_root)).await;

    Fixture {
        team_id,
        loud,
        quiet,
        user_id: plain.id,
        token: plain.token,
    }
}

async fn unwind(client: &reqwest::Client, admin_token: &str, f: &Fixture) {
    delete_channel(client, admin_token, &f.loud).await;
    delete_channel(client, admin_token, &f.quiet).await;
    delete_plain_user(client, admin_token, &f.user_id).await;
}

/// The body matches Go **byte for byte**, `me` resolves to the session's id, the trailing newline
/// is there, and the fold's four counters are pairwise different and equal to the sum of Go's own
/// per-channel answers for the two fixture channels.
#[tokio::test]
async fn the_body_matches_go_byte_for_byte_for_me_and_the_explicit_id() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let f = fixture(&client, &admin_token, "tsufold").await;

    let me_path = format!("/api/v4/users/me/teams/{}/unread", f.team_id);
    let (go_me, rs_me) = fetch_both_stable(&client, &f.token, &me_path).await;
    let explicit = format!("/api/v4/users/{}/teams/{}/unread", f.user_id, f.team_id);
    let (_, rs_explicit) = fetch_both_stable(&client, &f.token, &explicit).await;
    let loud_unread = go_channel_unread(&client, &f.token, &f.user_id, &f.loud).await;
    let quiet_unread = go_channel_unread(&client, &f.token, &f.user_id, &f.quiet).await;
    unwind(&client, &admin_token, &f).await;

    assert_eq!(
        String::from_utf8_lossy(&rs_me),
        String::from_utf8_lossy(&go_me),
        "a single struct in declaration order: the bytes must match exactly"
    );
    assert_eq!(rs_me, rs_explicit, "`me` resolves to the session's id");
    assert_eq!(
        rs_me.last(),
        Some(&b'\n'),
        "json.NewEncoder(w).Encode, not Marshal + Write — the plural sibling has no newline"
    );

    let team: serde_json::Value = serde_json::from_slice(&rs_me).expect("decodes");
    assert_eq!(team["team_id"].as_str(), Some(f.team_id.as_str()));
    assert_eq!(
        team.as_object().unwrap().len(),
        8,
        "model.TeamUnread has eight fields and none is omitempty: {team}"
    );
    for key in [
        "thread_count",
        "thread_mention_count",
        "thread_urgent_mention_count",
    ] {
        assert_eq!(
            team[key],
            serde_json::json!(0),
            "the singular handler has no threads half on either server"
        );
    }

    let msg = team["msg_count"].as_i64().unwrap();
    let msg_root = team["msg_count_root"].as_i64().unwrap();
    let mention = team["mention_count"].as_i64().unwrap();
    let mention_root = team["mention_count_root"].as_i64().unwrap();
    assert!(
        msg > msg_root && msg > mention && mention > mention_root,
        "four pairwise-different counters, or a swapped column could pass: {team}"
    );
    let mut summed = [0_i64; 4];
    for channel in [&loud_unread, &quiet_unread] {
        for (slot, key) in [
            "msg_count",
            "msg_count_root",
            "mention_count",
            "mention_count_root",
        ]
        .into_iter()
        .enumerate()
        {
            summed[slot] += channel[key].as_i64().unwrap();
        }
    }
    assert!(loud_unread["mention_count"].as_i64().unwrap() > 0);
    assert!(quiet_unread["mention_count"].as_i64().unwrap() > 0);
    assert_eq!(
        [mention, mention_root],
        [summed[2], summed[3]],
        "mentions come only from the two fixture channels, so the team sum is exactly their sum"
    );
    assert!(
        msg >= summed[0] && msg_root >= summed[1],
        "the team's default channels may add more, never less: {team}"
    );
    assert!(
        msg > loud_unread["msg_count"].as_i64().unwrap(),
        "a fold that stopped after the first row would land here: {team}"
    );
}

/// Muting one channel (`mark_unread = mention`) removes **that channel's** messages from the team
/// sum and nothing else — the other channel's messages stay, and every mention stays.
#[tokio::test]
async fn a_muted_channel_drops_its_messages_and_keeps_its_mentions() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let f = fixture(&client, &admin_token, "tsumute").await;
    let path = format!("/api/v4/users/me/teams/{}/unread", f.team_id);

    let (go_before, rs_before) = fetch_both_stable(&client, &f.token, &path).await;
    assert_eq!(rs_before, go_before);
    let before: serde_json::Value = serde_json::from_slice(&rs_before).expect("decodes");

    set_member_notify_prop(
        &client,
        &f.token,
        &f.loud,
        &f.user_id,
        "mark_unread",
        "mention",
    )
    .await;

    let (go_after, rs_after) = fetch_both_stable(&client, &f.token, &path).await;
    unwind(&client, &admin_token, &f).await;
    assert_eq!(
        rs_after, go_after,
        "the mute shortcut must fire identically on both servers"
    );
    let after: serde_json::Value = serde_json::from_slice(&rs_after).expect("decodes");

    let dropped = before["msg_count"].as_i64().unwrap() - after["msg_count"].as_i64().unwrap();
    assert_eq!(
        dropped, 3,
        "loud's three unread posts after the view are gone from the sum"
    );
    assert!(
        after["msg_count"].as_i64().unwrap() > 0,
        "quiet's messages still count: the mute is per row, not per team"
    );
    assert_eq!(
        after["mention_count"], before["mention_count"],
        "mentions pierce the mute"
    );
    assert_eq!(after["mention_count_root"], before["mention_count_root"]);
}

/// The **second** gate, which the plural sibling does not have at all: a team the caller is not a
/// member of is a 403 naming `view_team` — **even when asking about themselves**. An id that
/// matches no team at all is the same 403, so no team lookup is observable.
///
/// # Why this test owns two teams
///
/// Its served control has to byte-compare counters, and every other test in this binary is busy
/// adding users to the *shared* fixture team — each of which posts a join message into
/// `town-square`, which every member's team total then includes. Comparing on that team is a
/// race: it failed once under a **no-op mutation control**, which is how it was found. Both teams
/// here are created by this test and touched by nothing else.
#[tokio::test]
async fn a_team_the_caller_cannot_see_is_a_view_team_refusal_even_for_self() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let joined_id = create_team(&client, &admin_token, "tsujoined").await;
    let closed_id = create_team(&client, &admin_token, "tsuclosed").await;
    // The reader is put on `joined_id` and never on `closed_id`.
    let reader = create_plain_user(&client, &admin_token, &joined_id, "tsuteam").await;

    for (path, what) in [
        (
            format!("/api/v4/users/me/teams/{closed_id}/unread"),
            "a real team the caller is not in",
        ),
        (
            "/api/v4/users/me/teams/zzzzzzzzzzzzzzzzzzzzzzzzzz/unread".to_owned(),
            "a team id that matches no row",
        ),
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &reader.token, &path).await;
        assert_eq!(go_status, 403, "{what}");
        assert_eq!(rs_status, 403, "{what}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, what);
        assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));
    }

    // Both gates failing at once is still one 403, and an identical one. Which permission the
    // refusal names is **not** on the wire — `WipeDetailed` empties `detailed_error` outside dev
    // mode (utils.go:339) and `message` is the untranslated gap [D-092] — so the *order* of the
    // two checks is pinned in-process instead, by `teams::team_unread_denied`.
    let both = format!(
        "/api/v4/users/{}/teams/{closed_id}/unread",
        logged_in_user_id()
    );
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &reader.token, &both).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, 403);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "both gates fail");

    // The control: the same caller, the same route, on the team they *are* in — served.
    let mine = format!("/api/v4/users/me/teams/{joined_id}/unread");
    let (go_mine, rs_mine) = fetch_both_stable(&client, &reader.token, &mine).await;
    assert_eq!(rs_mine, go_mine, "the team gate grants for a member");

    // The same team the reader was refused, read by the admin: served. So the gate is about the
    // caller, not about the team — and a 403 above was not the team being unreadable to everyone.
    let theirs = format!("/api/v4/users/me/teams/{closed_id}/unread");
    let (go_admin, rs_admin) = fetch_both_stable(&client, &admin_token, &theirs).await;
    assert_eq!(rs_admin, go_admin);

    delete_plain_user(&client, &admin_token, &reader.id).await;
}

/// The **first** gate: `SessionHasPermissionToUser`. A plain user is refused another user's
/// badges on a team they can both see — so the refusal is the *user* gate, not the team one —
/// and an admin is served them.
///
/// The branch this cannot reach is step 5, "even `edit_other_users` cannot read a system admin":
/// no persisted system role in this database carries `edit_other_users` (checked: `system_manager`,
/// `system_user_manager` and `system_read_only_admin` all lack it), and Team Edition will not
/// assign one over REST. That branch rests on the unit tests in `mm-app/src/authorization.rs`.
#[tokio::test]
async fn another_users_badges_need_permission_to_that_user() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let team_id = create_team(&client, &admin_token, "tsugate").await;
    let reader = create_plain_user(&client, &admin_token, &team_id, "tsugate").await;

    // Plain user → the admin's badges: refused, and by the *user* gate, since the reader is a
    // member of this team and reads their own badges on it perfectly well (asserted below).
    let about_admin = format!(
        "/api/v4/users/{}/teams/{team_id}/unread",
        logged_in_user_id()
    );
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &reader.token, &about_admin).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, 403);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "plain about admin");
    assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));

    // The control that isolates the gate: the same caller, the same team, themselves.
    let about_self = format!("/api/v4/users/{}/teams/{team_id}/unread", reader.id);
    let (go_self, rs_self) = fetch_both_stable(&client, &reader.token, &about_self).await;
    assert_eq!(rs_self, go_self, "self on a team they can see is served");

    // Admin → the plain user's badges: served, and identical.
    let (go_other, rs_other) = fetch_both_stable(&client, &admin_token, &about_self).await;
    assert_eq!(
        rs_other, go_other,
        "the admin view of another user's badges must match"
    );
    assert_eq!(
        rs_other, rs_self,
        "the same rows either way — the gate decides who may ask, not what is counted"
    );

    delete_plain_user(&client, &admin_token, &reader.id).await;
}

/// A well-formed team id, a well-formed user id, nobody home: for an admin that is an **all-zero
/// object carrying the requested team id**, not a 404 and not an omission. This is the difference
/// from the plural route, which simply would not list the team.
#[tokio::test]
async fn a_team_with_nothing_unread_is_an_all_zero_object_not_a_miss() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/users/zzzzzzzzzzzzzzzzzzzzzzzzzz/teams/yyyyyyyyyyyyyyyyyyyyyyyyyy/unread";
    let (go_body, rs_body) = fetch_both_stable(&client, &token, path).await;
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        String::from_utf8_lossy(&rs_body),
        "{\"team_id\":\"yyyyyyyyyyyyyyyyyyyyyyyyyy\",\"msg_count\":0,\"mention_count\":0,\
         \"mention_count_root\":0,\"msg_count_root\":0,\"thread_count\":0,\
         \"thread_mention_count\":0,\"thread_urgent_mention_count\":0}\n",
        "the team id comes from the URL, and the field order is the struct's"
    );
}

/// A malformed id is a 400 from both servers, on the same parameter chain: `RequireTeamId()`
/// runs **before** `RequireUserId()`, and a segment outside `[A-Za-z0-9]+` never matches Go's mux
/// at all, so [D-150]'s forwarded 404 applies here too.
#[tokio::test]
async fn malformed_ids_answer_exactly_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    // Both segments alphanumeric but too short: Go's mux matches and the handler 400s.
    for path in [
        "/api/v4/users/me/teams/short/unread",
        "/api/v4/users/short/teams/zzzzzzzzzzzzzzzzzzzzzzzzzz/unread",
        "/api/v4/users/short/teams/short/unread",
    ] {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &token, path).await;
        assert_eq!(go_status, 400, "{path}");
        assert_eq!(rs_status, 400, "{path}");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, path);
    }

    // A hyphen is outside Go's `[A-Za-z0-9]+` mux class, so Go 404s from the router and we
    // forward rather than 400 from our own validator.
    let path = "/api/v4/users/me/teams/no-pe/unread";
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
        Some("go")
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

/// `include_collapsed_threads=true` is **not** a forward here: Go's singular handler never reads
/// the parameter, so the query string is inert on both servers and the answer is the served one.
#[tokio::test]
async fn the_collapsed_threads_parameter_is_inert_and_nothing_is_forwarded() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let team_id = create_team(&client, &admin_token, "tsuflag").await;

    let plain = format!("/api/v4/users/me/teams/{team_id}/unread");
    let with_flag = format!("{plain}?include_collapsed_threads=true");
    let (go_plain, rs_plain) = fetch_both_stable(&client, &admin_token, &plain).await;
    // `fetch_both_stable` asserts `x-mmrs-served-by: rust`, so this call *is* the assertion that
    // the flag does not forward.
    let (go_flag, rs_flag) = fetch_both_stable(&client, &admin_token, &with_flag).await;

    assert_eq!(rs_plain, go_plain);
    assert_eq!(rs_flag, go_flag);
    assert_eq!(
        rs_flag, rs_plain,
        "the singular handler has no collapsed-threads branch on either server"
    );
}
