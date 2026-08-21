//! Cross-server parity for `GET /api/v4/users` (`getUsers`).
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_users_list
//! ```
//!
//! # Five arms served, the rest forwarded
//!
//! The handler serves the unfiltered list, `in_team`, `in_channel`, `not_in_channel` and
//! `not_in_team`. Everything else goes to Go whole, by the same condition Go's own dispatch
//! uses — [`forwarded_variants`] walks every one of them and asserts the proxy answered, and the
//! served comparisons below are the other side of that boundary.
//!
//! # Why `update_at` is comparable here and was not for `POST /users/ids`
//!
//! `LocalCacheUserStore` wraps exactly three reads, and only one of them is on this route:
//! `GetAllProfiles` — and *only* when the options are all empty **and** `page=0&per_page=100`,
//! which is hardcoded to the webapp's call (user_layer.go:120). Every other query on this route,
//! including all four filtered arms, reaches Postgres directly on both servers, so the stale
//! post-login `update_at` that forced the `getUsersByIds` suite to patch every fixture user does
//! not arise. **The one cached shape is deliberately never compared here** — see the note in
//! `MIGRATION.md`.
//!
//! # Why the list comparisons go through `fetch_both_stable`
//!
//! The unfiltered and `not_in_team` arms list users this suite does not own, and three other
//! parity suites create and delete users concurrently against the same database. Reading
//! Go-ours-Go and requiring the two Go answers to agree is what makes a difference mean a
//! divergence rather than a race.

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_channel,
    create_plain_user, delete_plain_user, fetch_both_raw, fetch_both_stable, go_minted_token,
    purge_api_fixtures, stack_enabled,
};

const PATH: &str = "/api/v4/users";

struct Fixture {
    admin_token: String,
    /// A team created for this test alone, with the admin and [`Fixture::members`] in it.
    team: String,
    /// A channel on `team`, with the admin and `members[0]` in it. `members[1]` is in the team
    /// but not the channel — the row `not_in_channel` must return.
    channel: String,
    members: Vec<common::PlainUser>,
    /// A user in no team at all, so `not_in_team` has something of ours to find.
    outsider: common::PlainUser,
}

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
        "creating the fixture team failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the team decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn remove_from_team(client: &reqwest::Client, admin_token: &str, team: &str, user: &str) {
    let response = client
        .delete(format!("{GO}/api/v4/teams/{team}/members/{user}"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "removing {user} from {team} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

/// Go's unfiltered listing **500s** on any `Users` row with a NULL `Nickname`.
///
/// `GetAllProfiles` scans into `model.User` with `sqlx`, whose `string` destination cannot take
/// a NULL: `sql: Scan error on column index 10, name "nickname": converting NULL to string is
/// unsupported`, surfaced as `app.user.get_profiles.app_error`. This port reads the same row
/// happily (`unwrap_or_default`), so the two servers disagree — but only for a row no Mattermost
/// server would ever write, because Go's own `INSERT` fills every column.
///
/// The development database nevertheless acquires such rows: `mm-app`'s `db_authorization` suite
/// plants one that omits the three name columns and purges only at the *start*, so it survives
/// between runs. Under `cargo test --workspace` the `parity_*` binaries happen to run first and
/// never see it; a standalone re-run of this file after that suite hits Go's 500 and fails four
/// tests that are about something else entirely.
///
/// So this normalises rather than deletes: nothing another suite planted goes away, the columns
/// it did not set become what a real row would carry, and `UpdateAt` is untouched so no etag
/// moves. Skipped silently when `DATABASE_URL` is unset — the assertions below then fail on
/// their own terms.
async fn make_every_user_scannable_by_go() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
    else {
        return;
    };
    let _ = sqlx::query(
        "UPDATE users
            SET nickname = COALESCE(nickname, ''),
                firstname = COALESCE(firstname, ''),
                lastname = COALESCE(lastname, ''),
                position = COALESCE(position, ''),
                lastpictureupdate = COALESCE(lastpictureupdate, 0),
                mfausedtimestamps = COALESCE(mfausedtimestamps, 'null'::jsonb)
          WHERE nickname IS NULL
             OR firstname IS NULL
             OR lastname IS NULL
             OR position IS NULL
             OR lastpictureupdate IS NULL
             OR mfausedtimestamps IS NULL",
    )
    .execute(&pool)
    .await;
}

async fn fixture(client: &reqwest::Client, tag: &str) -> Fixture {
    purge_api_fixtures().await;
    make_every_user_scannable_by_go().await;
    let admin_token = go_minted_token(client).await;
    let team = create_team(client, &admin_token, tag).await;

    let mut members = Vec::new();
    for n in ["a", "b"] {
        members.push(create_plain_user(client, &admin_token, &team, &format!("{tag}{n}")).await);
    }
    let channel = create_channel(client, &admin_token, &team, tag).await;
    common::add_user_to_channel(client, &admin_token, &channel, &members[0].id).await;

    // Created inside the team so `create_plain_user` can log it in, then taken back out: the
    // team membership row is left behind with `DeleteAt != 0`, which is exactly the row that
    // tells `in_team` (excludes it) from `not_in_team` (includes it).
    let outsider = create_plain_user(client, &admin_token, &team, &format!("{tag}out")).await;
    remove_from_team(client, &admin_token, &team, &outsider.id).await;

    Fixture {
        admin_token,
        team,
        channel,
        members,
        outsider,
    }
}

/// Best-effort teardown. **The team goes too**, and that is not tidiness: a live
/// `mmrs-parity-*` team left behind becomes `teams[0]` for another suite's
/// `a_team_and_channel_the_user_is_in`, and a `purge_api_fixtures` from a third binary can then
/// delete it out from under that suite mid-test. An aborted run of this file did exactly that
/// and failed two tests in `parity_user_get` that have nothing to do with this route.
async fn teardown(client: &reqwest::Client, f: &Fixture) {
    for user in f.members.iter().chain(std::iter::once(&f.outsider)) {
        delete_plain_user(client, &f.admin_token, &user.id).await;
    }
    common::delete_channel(client, &f.admin_token, &f.channel).await;
    let _ = client
        .delete(format!("{GO}/api/v4/teams/{}", f.team))
        .header("Authorization", format!("Bearer {}", f.admin_token))
        .send()
        .await;
}

fn ids_of(body: &[u8], context: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "{context}: not JSON ({e}): {}",
            String::from_utf8_lossy(body)
        )
    });
    parsed
        .as_array()
        .unwrap_or_else(|| panic!("{context}: not an array"))
        .iter()
        .map(|u| u["id"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// GET a path from both servers and return `(status, served_by, etag)` for each.
async fn headers_of_both(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    if_none_match: Option<&str>,
) -> Vec<(u16, String, Option<String>)> {
    let mut out = Vec::new();
    for base in [GO, RUST] {
        let mut request = client
            .get(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"));
        if let Some(etag) = if_none_match {
            request = request.header("If-None-Match", etag);
        }
        let response = request
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let etag = response
            .headers()
            .get("ETag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        out.push((response.status().as_u16(), served_by, etag));
    }
    out
}

/// The four filtered arms, byte for byte, with the fixture built so each one returns a
/// *different* set: a member of the channel, a member of the team who is not, and a former
/// member of the team who is in neither.
#[tokio::test]
async fn the_four_filtered_arms_match_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "ulfour").await;
    let token = f.admin_token.clone();

    let in_team = format!("{PATH}?in_team={}&per_page=200", f.team);
    let in_channel = format!("{PATH}?in_channel={}&per_page=200", f.channel);
    let not_in_channel = format!(
        "{PATH}?in_team={}&not_in_channel={}&per_page=200",
        f.team, f.channel
    );
    let not_in_team = format!("{PATH}?not_in_team={}&per_page=200", f.team);

    let (go_team, rs_team) = fetch_both_stable(&client, &token, &in_team).await;
    let (go_chan, rs_chan) = fetch_both_stable(&client, &token, &in_channel).await;
    let (go_not_chan, rs_not_chan) = fetch_both_stable(&client, &token, &not_in_channel).await;
    let (go_not_team, rs_not_team) = fetch_both_stable(&client, &token, &not_in_team).await;
    teardown(&client, &f).await;

    assert_eq!(rs_team, go_team, "in_team");
    assert_eq!(rs_chan, go_chan, "in_channel");
    assert_eq!(rs_not_chan, go_not_chan, "not_in_channel");
    assert_eq!(rs_not_team, go_not_team, "not_in_team");
    assert_ne!(rs_team.last(), Some(&b'\n'), "Marshal appends nothing");

    // The fixture has to make the four arms disagree, or a handler that ran the same query four
    // times would pass every assertion above.
    let team = ids_of(&rs_team, "in_team");
    let chan = ids_of(&rs_chan, "in_channel");
    let not_chan = ids_of(&rs_not_chan, "not_in_channel");
    let not_team = ids_of(&rs_not_team, "not_in_team");

    assert!(team.contains(&f.members[0].id) && team.contains(&f.members[1].id));
    assert!(
        !team.contains(&f.outsider.id),
        "the outsider's TeamMembers row is soft-deleted, so in_team must not list it"
    );
    assert!(chan.contains(&f.members[0].id) && !chan.contains(&f.members[1].id));
    assert!(
        !not_chan.contains(&f.members[0].id) && not_chan.contains(&f.members[1].id),
        "not_in_channel is the team minus the channel"
    );
    assert!(
        !not_chan.contains(&f.outsider.id),
        "not_in_channel is scoped to in_team, so a non-member is not a candidate"
    );
    assert!(
        not_team.contains(&f.outsider.id) && !not_team.contains(&f.members[0].id),
        "the same soft-deleted membership row that hides the outsider from in_team reveals it \
         here"
    );
}

/// The unfiltered arm. Deliberately at `per_page=200`, never at Go's cached `page=0&per_page=100`.
#[tokio::test]
async fn the_unfiltered_list_matches_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "ulall").await;

    let (go, rs) =
        fetch_both_stable(&client, &f.admin_token, &format!("{PATH}?per_page=200")).await;
    // No query string at all is the same arm with the default paging.
    let (go_bare, rs_bare) = fetch_both_stable(&client, &f.admin_token, PATH).await;
    let ids = ids_of(&rs, "unfiltered");
    teardown(&client, &f).await;

    assert_eq!(rs, go);
    assert_eq!(rs_bare, go_bare, "no query string, the same arm");
    assert!(
        ids.contains(&f.outsider.id) && ids.contains(&f.members[0].id),
        "the unfiltered list is not scoped by membership"
    );
}

/// Paging and the two `DeleteAt` filters, on the arm where the filters actually apply.
#[tokio::test]
async fn paging_and_the_active_filters_match_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "ulpage").await;
    let token = f.admin_token.clone();
    // A deactivated member, so `active` and `inactive` return different sets rather than the
    // same one — two filters that agree cannot tell a swapped predicate from a correct one.
    delete_plain_user(&client, &token, &f.members[1].id).await;

    let base = format!("{PATH}?in_team={}", f.team);
    let mut walked: Vec<String> = Vec::new();
    for page in 0..4 {
        let path = format!("{base}&per_page=1&page={page}");
        let (go, rs) = fetch_both_stable(&client, &token, &path).await;
        assert_eq!(rs, go, "page {page}");
        walked.extend(ids_of(&rs, "page"));
    }

    let (go_all, rs_all) =
        fetch_both_stable(&client, &token, &format!("{base}&per_page=200")).await;
    let (go_live, rs_live) =
        fetch_both_stable(&client, &token, &format!("{base}&per_page=200&active=true")).await;
    let (go_gone, rs_gone) = fetch_both_stable(
        &client,
        &token,
        &format!("{base}&per_page=200&inactive=true"),
    )
    .await;
    let (go_zero, rs_zero) =
        fetch_both_stable(&client, &token, &format!("{base}&per_page=0")).await;
    teardown(&client, &f).await;

    assert_eq!(rs_all, go_all);
    assert_eq!(rs_live, go_live, "active=true");
    assert_eq!(rs_gone, go_gone, "inactive=true");
    assert_eq!(rs_zero, go_zero, "per_page=0");
    assert_eq!(
        rs_zero, b"[]",
        "per_page=0 is LIMIT 0 here, not `everything`"
    );

    let all = ids_of(&rs_all, "all");
    assert_eq!(
        walked,
        all[..walked.len().min(all.len())],
        "a per_page=1 walk reassembles the single page — so OFFSET is page * per_page"
    );
    let live = ids_of(&rs_live, "live");
    let gone = ids_of(&rs_gone, "gone");
    assert!(
        gone.contains(&f.members[1].id) && !live.contains(&f.members[1].id),
        "the deactivated member separates the two filters: live {live:?} gone {gone:?}"
    );
    assert!(live.contains(&f.members[0].id) && !gone.contains(&f.members[0].id));
    assert_eq!(
        live.len() + gone.len(),
        all.len(),
        "the two filters partition the unfiltered list"
    );
}

/// [`headers_of_both`], read Go-ours-Go until Go's two answers agree.
///
/// The `not_in_team` etag is `MAX(UpdateAt)` over **every** user outside the team, so any other
/// test in this binary — or in another worktree — that touches any user row moves it between the
/// two reads. Measured: 108 milliseconds apart, same count, different MAX.
async fn stable_headers_of_both(
    client: &reqwest::Client,
    token: &str,
    path: &str,
) -> Vec<(u16, String, Option<String>)> {
    for attempt in 1..=8u64 {
        let before = headers_of_both(client, token, path, None).await;
        let ours = headers_of_both(client, token, path, None).await;
        let after = headers_of_both(client, token, path, None).await;
        if before[0] == after[0] {
            return vec![before[0].clone(), ours[1].clone()];
        }
        tokio::time::sleep(std::time::Duration::from_millis(50 * attempt)).await;
    }
    panic!("{path}: Go's etag never settled, so no comparison here would mean anything");
}

/// The etag arms. `in_team` mints one and 304s on it; `in_channel` never sends one; the
/// `not_in_team` etag is computed from **`in_team`**, which is the parameter Go passes; and the
/// two servers' etags differ in exactly the two components Go builds out of a `*bool` without
/// dereferencing it.
#[tokio::test]
async fn the_etag_arms_match_go_except_for_gos_two_pointer_components() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "uletag").await;
    let token = f.admin_token.clone();

    let in_team = format!("{PATH}?in_team={}&per_page=200", f.team);
    let in_channel = format!("{PATH}?in_channel={}&per_page=200", f.channel);
    let not_in_team = format!("{PATH}?not_in_team={}&per_page=200", f.team);
    let not_in_team_with_team = format!(
        "{PATH}?not_in_team={}&in_team={}&per_page=200",
        f.team, f.team
    );

    let team = stable_headers_of_both(&client, &token, &in_team).await;
    let chan = stable_headers_of_both(&client, &token, &in_channel).await;
    let not_team = stable_headers_of_both(&client, &token, &not_in_team).await;
    let not_team_scoped = stable_headers_of_both(&client, &token, &not_in_team_with_team).await;

    let go_etag = team[0].2.clone().expect("Go sends an ETag for in_team");
    let rs_etag = team[1].2.clone().expect("we send an ETag for in_team");
    let go_conditional = headers_of_both(&client, &token, &in_team, Some(&go_etag)).await;
    let rs_conditional = headers_of_both(&client, &token, &in_team, Some(&rs_etag)).await;
    let mismatched = headers_of_both(&client, &token, &in_team, Some("nonsense")).await;
    teardown(&client, &f).await;

    assert_eq!(team[1].1, "rust", "in_team must be served, not forwarded");
    assert_eq!(chan[0].2, None, "Go sends no etag on the in_channel arm");
    assert_eq!(chan[1].2, None);

    // Four dot-joined parts, and Go's version prefix contains dots of its own — so compare
    // from the right: [.., show_full_name, show_email_address, restrictions_hash].
    let parts = |etag: &str| -> Vec<String> { etag.rsplitn(4, '.').map(str::to_owned).collect() };
    let go_parts = parts(&go_etag);
    let rs_parts = parts(&rs_etag);
    assert_eq!(
        go_parts[0], "",
        "nil restrictions hash to an empty trailing component: {go_etag}"
    );
    assert_eq!(rs_parts[0], "");
    assert_eq!(
        go_parts[3], rs_parts[3],
        "the store half — version and MAX(UpdateAt) — must agree exactly: {go_etag} vs {rs_etag}"
    );
    assert_eq!(
        (rs_parts[1].as_str(), rs_parts[2].as_str()),
        ("true", "true"),
        "we render the privacy flags' values"
    );
    assert!(
        go_parts[1].starts_with("0x") && go_parts[2].starts_with("0x"),
        "Go renders the *addresses* of two `*bool`s it forgot to dereference \
         (app/users/users.go:184) — if this ever stops being true, upstream fixed the bug and \
         this port should drop its divergence: {go_etag}"
    );

    // The `not_in_team` etag reads `in_team`, so adding it changes the answer on Go itself.
    for (label, pair) in [("plain", &not_team), ("with in_team", &not_team_scoped)] {
        assert_eq!(
            parts(&pair[0].2.clone().unwrap_or_default())[3],
            parts(&pair[1].2.clone().unwrap_or_default())[3],
            "{label}: the store half of the not_in_team etag"
        );
    }
    assert_ne!(
        not_team[0].2, not_team_scoped[0].2,
        "adding in_team changes the not_in_team etag on Go itself — the handler passes in_team, \
         not not_in_team, to GetUsersNotInTeamEtag (api4/user.go:1049). Passing the \
         obviously-intended parameter would make these two equal."
    );
    assert_ne!(
        not_team[1].2, not_team_scoped[1].2,
        "and the port reproduces that, or a client would 304 on a stale list"
    );

    // Each server 304s on its own etag and 200s on the other's.
    assert_eq!(go_conditional[0].0, 304, "Go 304s on the etag Go minted");
    assert_eq!(
        go_conditional[1].0, 200,
        "we cannot 304 on an etag containing Go's heap addresses"
    );
    assert_eq!(rs_conditional[1].0, 304, "we 304 on the etag we minted");
    assert_eq!(
        rs_conditional[1].2,
        Some(rs_etag),
        "the 304 carries the etag"
    );
    assert_eq!(
        (mismatched[0].0, mismatched[1].0),
        (200, 200),
        "a plain string compare — a non-matching value is not a 304"
    );
}

/// Everything the handler refuses, compared against Go's own body.
#[tokio::test]
async fn refusals_match_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "ulrefu").await;
    // The outsider is in no team and in no channel, so both gates can refuse it — the admin
    // holds `manage_system`, which grants at the last branch of every permission check.
    let stranger = f.outsider.token.clone();

    let cases = [
        (
            f.admin_token.clone(),
            format!("{PATH}?not_in_channel={}", f.channel),
            400,
            "api.context.invalid_url_param.app_error",
            "not_in_channel without in_team",
        ),
        (
            stranger.clone(),
            format!("{PATH}?in_team={}", f.team),
            403,
            "api.context.permissions.app_error",
            "in_team for a team the caller is not in",
        ),
        (
            stranger.clone(),
            format!("{PATH}?not_in_team={}", f.team),
            403,
            "api.context.permissions.app_error",
            "not_in_team for a team the caller is not in",
        ),
        (
            stranger.clone(),
            format!("{PATH}?in_channel={}", f.channel),
            403,
            "api.context.permissions.app_error",
            "in_channel for a channel the caller is not in",
        ),
        (
            stranger.clone(),
            format!("{PATH}?in_team={}&not_in_channel={}", f.team, f.channel),
            403,
            "api.context.permissions.app_error",
            "not_in_channel for a channel the caller is not in",
        ),
    ];

    for (token, path, expected_status, expected_id, context) in &cases {
        let ((go_status, go_body), (rs_status, rs_body)) =
            fetch_both_raw(&client, token, path).await;
        assert_eq!(go_status, *expected_status, "{context}: Go's status");
        assert_eq!(rs_status, *expected_status, "{context}: our status");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, context);
        assert_eq!(go["id"], *expected_id, "{context}");
    }
    teardown(&client, &f).await;
}

/// The forwarding boundary, from both sides: each query Go dispatches to code this port does
/// not have must come back marked `go`, and its near neighbour must come back marked `rust`.
#[tokio::test]
async fn forwarded_variants() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "ulfwd").await;
    let token = f.admin_token.clone();
    let team = f.team.clone();
    let channel = f.channel.clone();

    let forwarded = [
        format!("{PATH}?in_group=zzzzzzzzzzzzzzzzzzzzzzzzzz"),
        format!("{PATH}?not_in_group=zzzzzzzzzzzzzzzzzzzzzzzzzz"),
        format!("{PATH}?without_team=true"),
        format!("{PATH}?in_team={team}&sort=last_activity_at"),
        format!("{PATH}?in_team={team}&sort=create_at"),
        format!("{PATH}?in_channel={channel}&sort=status"),
        format!("{PATH}?in_channel={channel}&sort=admin"),
        format!("{PATH}?in_team={team}&sort=nonsense"),
        format!("{PATH}?in_team={team}&role=system_admin"),
        format!("{PATH}?in_team={team}&roles=system_user"),
        format!("{PATH}?in_channel={channel}&channel_roles=channel_admin"),
        format!("{PATH}?in_team={team}&team_roles=team_admin"),
        format!("{PATH}?not_in_team={team}&group_constrained=true"),
        format!("{PATH}?in_team={team}&not_in_channel={channel}&group_constrained=true"),
        format!("{PATH}?not_in_team={team}&abac_match_only=true"),
        format!("{PATH}?in_team={team}&active=true&inactive=true"),
    ];
    // Each one differs from a forwarded neighbour by exactly the thing the rule is about.
    let served = [
        format!("{PATH}?in_team={team}&in_group=zzzzzzzzzzzzzzzzzzzzzzzzzz"),
        format!("{PATH}?without_team=false"),
        format!("{PATH}?in_team={team}&sort="),
        format!("{PATH}?in_team={team}&role="),
        format!("{PATH}?in_team={team}&group_constrained=true&abac_match_only=true"),
        format!("{PATH}?not_in_team={team}&group_constrained=false"),
        format!("{PATH}?in_team={team}&active=true&inactive=false"),
    ];

    let mut verdicts = Vec::new();
    for path in forwarded.iter().chain(served.iter()) {
        let both = headers_of_both(&client, &token, path, None).await;
        verdicts.push((path.clone(), both[0].0, both[1].0, both[1].1.clone()));
    }
    teardown(&client, &f).await;

    for (path, go_status, rs_status, served_by) in &verdicts {
        let expected = if forwarded.contains(path) {
            "go"
        } else {
            "rust"
        };
        assert_eq!(served_by, expected, "{path}");
        assert_eq!(go_status, rs_status, "{path}: the two servers' statuses");
    }
}

/// A non-admin caller: the whole list goes through the strict `SanitizeProfile`, including the
/// caller's own row, exactly as on `POST /users/ids`.
#[tokio::test]
async fn a_plain_caller_gets_the_non_admin_sanitisation() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    let client = client();
    let f = fixture(&client, "ulsan").await;
    let path = format!("{PATH}?in_channel={}&per_page=200", f.channel);

    let (go, rs) = fetch_both_stable(&client, &f.members[0].token, &path).await;
    let (go_admin, _) = fetch_both_stable(&client, &f.admin_token, &path).await;
    teardown(&client, &f).await;

    assert_eq!(rs, go);
    let plain: serde_json::Value = serde_json::from_slice(&rs).expect("json");
    let admin: serde_json::Value = serde_json::from_slice(&go_admin).expect("json");
    let me = plain
        .as_array()
        .expect("array")
        .iter()
        .find(|u| u["id"] == f.members[0].id.as_str())
        .expect("the caller's own row is in the channel list");
    assert!(
        me.get("notify_props").is_none() && me.get("auth_data").is_some(),
        "no self exception on this route: the caller's own row is sanitised as `other` — {me}"
    );
    let admin_row = admin
        .as_array()
        .expect("array")
        .iter()
        .find(|u| u["id"] == f.members[0].id.as_str())
        .expect("the same row through the admin");
    assert!(
        admin_row.get("notify_props").is_some() && admin_row.get("auth_data").is_none(),
        "the admin override must make an observable difference, or this proves nothing about \
         IsSystemAdmin — {admin_row}"
    );
}
