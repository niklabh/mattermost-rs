//! Cross-server parity for `GET /api/v4/users/{user_id}/teams/unread`.
//!
//! ```sh
//! docker compose up -d && cargo run -p mm-api
//! MM_PARITY_STACK=1 cargo test -p mm-api --test parity_teams_unread
//! ```
//!
//! # Why nothing here compares bytes
//!
//! Go assembles the answer out of a `map[string]*TeamUnread` and appends in iteration order,
//! which Go randomises per request. Two consecutive Go answers for a user on two teams disagree
//! about half the time, so the byte-for-byte assertion every sibling suite makes is unavailable
//! and [`common::fetch_both_stable`] would never settle. [`fetch_both_sorted`] normalises both
//! sides to the list sorted by `team_id` and compares the `serde_json::Value`s; the one thing
//! that costs is the trailing-newline check, which is made on the raw bytes separately.
//!
//! Every fixture is created and unwound here; [`common::purge_api_fixtures`] clears what a
//! panicking run leaves behind.

mod common;

use common::{
    GO, RUST, a_team_and_channel_the_user_is_in, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel, create_plain_user,
    delete_channel, delete_plain_user, fetch_both_raw, go_minted_token, logged_in_user_id,
    post_message, purge_api_fixtures, set_member_notify_prop, stack_enabled, username_of,
    view_channel,
};

/// The list sorted by `team_id`, as a `Value` — the order-free shape of the answer.
fn sorted(body: &[u8]) -> Vec<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_slice(body).expect("decodes");
    let mut list = parsed.as_array().expect("an array").clone();
    list.sort_by(|a, b| a["team_id"].as_str().cmp(&b["team_id"].as_str()));
    list
}

/// Fetch from both servers, assert Rust served it and both answered 200, and return the two
/// **sorted** lists plus Rust's raw bytes. Go is read before and after and the two must agree as
/// sets — the same settle-check `fetch_both_stable` makes, with order taken out of it.
async fn fetch_both_sorted(
    client: &reqwest::Client,
    token: &str,
    path: &str,
) -> (Vec<serde_json::Value>, Vec<serde_json::Value>, Vec<u8>) {
    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        assert_eq!(response.status(), 200, "{base}{path} should return 200");
        if base == RUST {
            assert_eq!(
                response
                    .headers()
                    .get("x-mmrs-served-by")
                    .and_then(|v| v.to_str().ok()),
                Some("rust"),
                "{path} was forwarded to Go, so this comparison proves nothing"
            );
        }
        response.bytes().await.expect("body reads").to_vec()
    };

    for attempt in 1..=8_u64 {
        let before = sorted(&get(GO).await);
        let ours = get(RUST).await;
        let after = sorted(&get(GO).await);
        if before == after {
            return (before, sorted(&ours), ours);
        }
        tokio::time::sleep(std::time::Duration::from_millis(50 * attempt)).await;
    }
    panic!("{path}: Go's answer never settled as a set, so no comparison would mean anything");
}

fn entry<'a>(list: &'a [serde_json::Value], team_id: &str) -> &'a serde_json::Value {
    list.iter()
        .find(|t| t["team_id"].as_str() == Some(team_id))
        .unwrap_or_else(|| panic!("team {team_id} is in the answer: {list:?}"))
}

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

/// Make `user_id` a `system_read_only_admin` — a caller holding
/// `sysconsole_read_user_management_users` but **not** `manage_system` — and log them in again
/// so the new session carries the role. (`system_user_manager` would read the same in role.go,
/// but the **persisted** `Roles` row in this database does not carry the permission; the
/// read-only admin's does, measured.) Team Edition refuses to assign system roles over REST
/// (`api.user.update_user_roles.license.app_error`), so the column is written directly, the way
/// the harness already reaches into the shared database to purge. `None` without `DATABASE_URL`.
async fn relogin_as_system_read_only_admin(
    client: &reqwest::Client,
    user_id: &str,
    username: &str,
) -> Option<String> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .ok()?;
    sqlx::query("UPDATE users SET roles = 'system_user system_read_only_admin' WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("the role column is written");

    let response = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username, "password": "Mmrs-Plain-1234" }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "re-login failed");
    Some(
        response
            .headers()
            .get("Token")
            .and_then(|v| v.to_str().ok())
            .expect("a session token")
            .to_owned(),
    )
}

struct Fixture {
    team_id: String,
    loud: String,
    quiet: String,
    user_id: String,
    token: String,
}

/// A fresh non-admin on the fixture team, in two fresh channels, with traffic shaped so that the
/// **team** totals are sums the fold has to get right:
///
/// - `loud`: two root posts, the reader catches up (so `TotalMsgCount - MsgCount` is a real
///   subtraction), then a mentioning root, a mentioning reply and a plain root.
/// - `quiet`: one mentioning root post and one plain reply to it — a second channel with its own
///   distinct four counters, so a fold that stops after the first row, or counts one row twice,
///   lands on a different sum than Go.
async fn fixture(client: &reqwest::Client, admin_token: &str, tag: &str) -> Fixture {
    let (team_id, _) = a_team_and_channel_the_user_is_in(client, admin_token).await;
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

/// `me` and the explicit id agree with Go as sets, the body is `json.Marshal` + `w.Write` with
/// no trailing newline, and the fold produced four **pairwise different** team counters.
#[tokio::test]
async fn the_fold_matches_go_for_me_and_the_explicit_id() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let f = fixture(&client, &admin_token, "tufold").await;

    let (go_me, rs_me, raw_me) =
        fetch_both_sorted(&client, &f.token, "/api/v4/users/me/teams/unread").await;
    let explicit = format!("/api/v4/users/{}/teams/unread", f.user_id);
    let (_, rs_explicit, _) = fetch_both_sorted(&client, &f.token, &explicit).await;
    let loud_unread = go_channel_unread(&client, &f.token, &f.user_id, &f.loud).await;
    let quiet_unread = go_channel_unread(&client, &f.token, &f.user_id, &f.quiet).await;
    unwind(&client, &admin_token, &f).await;

    assert_eq!(rs_me, go_me, "the two servers must agree as sets");
    assert_eq!(rs_me, rs_explicit, "`me` resolves to the session's id");
    assert_ne!(
        raw_me.last(),
        Some(&b'\n'),
        "json.Marshal + w.Write, no encoder"
    );

    let team = entry(&rs_me, &f.team_id);
    let msg = team["msg_count"].as_i64().unwrap();
    let msg_root = team["msg_count_root"].as_i64().unwrap();
    let mention = team["mention_count"].as_i64().unwrap();
    let mention_root = team["mention_count_root"].as_i64().unwrap();
    assert!(
        msg > msg_root && msg > mention && mention > mention_root,
        "four pairwise-different team counters, or a swapped column could pass: {team}"
    );
    // The fold is a sum over channels, and Go's per-channel route is the oracle for each term:
    // the team's four counters must be at least the two fixture channels' added together (the
    // team's default channels may contribute more, never less), and strictly more than either
    // one alone — so a fold that stops after the first row is caught.
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
        "mentions only come from the two fixture channels: the team sum is exactly their sum"
    );
    assert!(msg >= summed[0] && msg_root >= summed[1], "{team}");
    for key in [
        "thread_count",
        "thread_mention_count",
        "thread_urgent_mention_count",
    ] {
        assert_eq!(
            team[key],
            serde_json::json!(0),
            "served without the threads half"
        );
    }
    assert_eq!(
        team.as_object().unwrap().len(),
        8,
        "model.TeamUnread has eight fields and none is omitempty: {team}"
    );
}

/// Muting one channel (`mark_unread = mention`) removes **that channel's** messages from the team
/// sum and nothing else: the other channel's messages stay, and every mention stays.
#[tokio::test]
async fn a_muted_channel_drops_its_messages_from_the_team_and_keeps_its_mentions() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let f = fixture(&client, &admin_token, "tumute").await;
    let path = "/api/v4/users/me/teams/unread";

    let (go_before, rs_before, _) = fetch_both_sorted(&client, &f.token, path).await;
    assert_eq!(rs_before, go_before);
    let before = entry(&rs_before, &f.team_id).clone();

    set_member_notify_prop(
        &client,
        &f.token,
        &f.loud,
        &f.user_id,
        "mark_unread",
        "mention",
    )
    .await;

    let (go_after, rs_after, _) = fetch_both_sorted(&client, &f.token, path).await;
    unwind(&client, &admin_token, &f).await;
    assert_eq!(
        rs_after, go_after,
        "the shortcut must fire identically on both servers"
    );
    let after = entry(&rs_after, &f.team_id);

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

/// `exclude_team` drops that team, and because the predicate is an unconditional `TeamId <> ?`
/// it **admits the team-less direct channels** as an entry with `team_id: ""`. Without it the
/// same DMs are hidden. Both halves measured against Go.
#[tokio::test]
async fn exclude_team_drops_the_team_and_surfaces_the_direct_channels_as_an_empty_team_id() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let f = fixture(&client, &admin_token, "tuexcl").await;

    // A DM with unread traffic from the admin.
    let response = client
        .post(format!("{GO}/api/v4/channels/direct"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!([logged_in_user_id(), f.user_id]))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success());
    let dm: serde_json::Value = response.json().await.expect("the DM decodes");
    let dm_id = dm["id"].as_str().expect("an id").to_owned();
    post_message(&client, &admin_token, &dm_id, "psst", None).await;
    post_message(&client, &admin_token, &dm_id, "psst again", None).await;

    let (go_all, rs_all, _) =
        fetch_both_sorted(&client, &f.token, "/api/v4/users/me/teams/unread").await;
    let excluded = format!("/api/v4/users/me/teams/unread?exclude_team={}", f.team_id);
    let (go_excl, rs_excl, _) = fetch_both_sorted(&client, &f.token, &excluded).await;
    unwind(&client, &admin_token, &f).await;

    assert_eq!(rs_all, go_all);
    assert_eq!(rs_excl, go_excl);

    assert!(
        rs_all
            .iter()
            .any(|t| t["team_id"].as_str() == Some(f.team_id.as_str())),
        "the home team is listed without an exclusion"
    );
    assert!(
        !rs_all.iter().any(|t| t["team_id"].as_str() == Some("")),
        "`TeamId <> ''` hides the DM by default: {rs_all:?}"
    );

    assert!(
        !rs_excl
            .iter()
            .any(|t| t["team_id"].as_str() == Some(f.team_id.as_str())),
        "the excluded team is gone"
    );
    let dm_entry = entry(&rs_excl, "");
    assert_eq!(
        dm_entry["msg_count"].as_i64().unwrap(),
        2,
        "the two DM posts surface under the empty team id: {dm_entry}"
    );
}

/// Reading another user's badges needs `manage_system` — **not** the sysconsole-read permission
/// the `/teams` sibling accepts. A plain user is a 403 from both servers; an admin is served.
#[tokio::test]
async fn asking_about_another_user_needs_manage_system() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin_token).await;
    let reader = create_plain_user(&client, &admin_token, &team_id, "tugate").await;

    let about_admin = format!("/api/v4/users/{}/teams/unread", logged_in_user_id());
    let ((go_status, go_body), (rs_status, rs_body)) =
        fetch_both_raw(&client, &reader.token, &about_admin).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, 403);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "plain about admin");
    assert_eq!(go["id"].as_str(), Some("api.context.permissions.app_error"));

    let about_reader = format!("/api/v4/users/{}/teams/unread", reader.id);
    let (go_list, rs_list, _) = fetch_both_sorted(&client, &admin_token, &about_reader).await;
    assert_eq!(
        rs_list, go_list,
        "the admin view of another user's badges must match"
    );

    // **The constant the `/teams` sibling accepts is refused here.** A `system_read_only_admin`
    // holds `sysconsole_read_user_management_users` and can list the admin's teams, but not the
    // admin's badges — the one actor who can tell `manage_system` from its plausible neighbour.
    let username = username_of(&client, &admin_token, &reader.id).await;
    if let Some(manager_token) =
        relogin_as_system_read_only_admin(&client, &reader.id, &username).await
    {
        let teams = client
            .get(format!("{GO}/api/v4/users/{}/teams", logged_in_user_id()))
            .header("Authorization", format!("Bearer {manager_token}"))
            .send()
            .await
            .expect("Go answers");
        assert_eq!(
            teams.status(),
            200,
            "the control: the sysconsole reader can list the admin's teams"
        );
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, &manager_token, &about_admin).await;
        assert_eq!(go_status, 403, "…and is refused the admin's badges by Go");
        assert_eq!(
            rs_status, 403,
            "…and by Rust: the gate is manage_system, not sysconsole"
        );
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "manager about admin");
    } else {
        eprintln!("DATABASE_URL unset: the sysconsole-reader half of this test did not run");
    }
    delete_plain_user(&client, &admin_token, &reader.id).await;
}

/// `include_collapsed_threads=true` is **forwarded** — the Threads store is not ported — and
/// only the literal `true` is: `=1` is served from Rust, as Go's string compare would ignore it.
#[tokio::test]
async fn the_collapsed_threads_variant_is_forwarded_and_only_for_the_literal_true() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let forwarded = client
        .get(format!(
            "{RUST}/api/v4/users/me/teams/unread?include_collapsed_threads=true"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the Rust server answers");
    assert_eq!(
        forwarded
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "the threads half is Go's"
    );
    assert_eq!(forwarded.status(), 200);

    let (go_one, rs_one, _) = fetch_both_sorted(
        &client,
        &token,
        "/api/v4/users/me/teams/unread?include_collapsed_threads=1",
    )
    .await;
    assert_eq!(rs_one, go_one);
}

/// A well-formed id that matches no user is, for an admin, an **empty list** — no user lookup.
#[tokio::test]
async fn a_nonexistent_user_is_an_empty_list_for_an_admin() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let path = "/api/v4/users/zzzzzzzzzzzzzzzzzzzzzzzzzz/teams/unread";
    let (go_list, rs_list, raw) = fetch_both_sorted(&client, &token, path).await;
    assert_eq!(rs_list, go_list);
    assert_eq!(raw, b"[]", "an empty list, not `null` and not a 404");
}

/// [D-150]: a segment outside `[A-Za-z0-9]+` never matches Go's route, so the mux 404 comes from
/// Go rather than a 400 from our `IsValidId`.
#[tokio::test]
async fn a_non_alphanumeric_segment_answers_exactly_as_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;

    let client = client();
    let token = go_minted_token(&client).await;
    let path = "/api/v4/users/no-pe/teams/unread";

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
