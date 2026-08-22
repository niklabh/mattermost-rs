//! `GET /api/v4/users/autocomplete` against the running Go server.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_users_autocomplete
//! ```
//!
//! Three arms with three different response *shapes*, a 500 that is not a 400, two permission
//! gates whose order is observable, and a limit block with three failure modes. Everything here
//! is a byte comparison against Go on the same database, because the questions this route raises
//! — is `out_of_channel` `[]` or absent, is there a trailing newline, does `agents` appear —
//! are all questions about bytes.
//!
//! # The fixture is built once and searched by prefix
//!
//! Three sibling worktrees create users in this same database while this runs, so **no
//! assertion here is about an unfiltered result set**. Every request carries
//! `name=mmrsplainautoc`, a prefix only this suite's users have, and the assertions are about
//! that set alone. A previous session shipped a suite that passed only because nobody else was
//! writing; the no-op mutation controls are what caught it.
//!
//! # Teardown is at the *start*, not the end
//!
//! `common::purge_api_fixtures` sweeps every `mmrsplain%` user and `mmrs-parity-%` channel
//! before anything is created, which is this project's convention and the only one that works:
//! a failing assertion panics past any trailing cleanup, and the harness runs the tests in a
//! binary concurrently, so a teardown *test* would delete another test's fixtures mid-run. Both
//! the users and the two channels this suite creates carry those prefixes.

mod common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, fetch_both, fetch_both_raw,
    go_minted_token, purge_api_fixtures,
};

/// The prefix every fixture user of this suite shares, and the search term of every request.
/// `mmrsplain…` because that is what `common::purge_api_fixtures` sweeps up.
const PREFIX: &str = "mmrsplainautoc";

/// A token that exists **only** in one user's email address. "Never autocomplete on emails"
/// (api4/user.go:1399) is the assertion that this finds nobody.
const EMAIL_ONLY_TOKEN: &str = "autocemailonlytoken";

const PASSWORD: &str = "Mmrs-Plain-1234";

struct Fixture {
    team_id: String,
    channel_id: String,
    /// A private channel on the same team that `caller` is **not** a member of.
    private_channel_id: String,
    /// In the team and in the channel. Also the non-admin caller — it holds its own token.
    caller_id: String,
    caller_token: String,
    /// In the team, not in the channel. Named for the reader; the assertions reach it through
    /// its username, not its id.
    #[allow(dead_code)]
    outsider_id: String,
    /// In the team, not in the channel, and the one whose address carries [`EMAIL_ONLY_TOKEN`].
    #[allow(dead_code)]
    email_id: String,
    admin_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(build_fixture).await
}

/// Create a user with **every searchable column populated**, through Go's own API.
///
/// Go's `POST /users` is the only writer here on purpose: a row inserted straight into Postgres
/// would skip `PreSave`, and then the two servers would be reading a row neither of them would
/// have written.
async fn create_user(
    client: &reqwest::Client,
    admin_token: &str,
    username: &str,
    email: &str,
    first: &str,
    last: &str,
    nickname: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/users"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "email": email,
            "username": username,
            "password": PASSWORD,
            "first_name": first,
            "last_name": last,
            "nickname": nickname,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating {username} failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the user decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn add_to_team(client: &reqwest::Client, admin_token: &str, team_id: &str, user_id: &str) {
    let response = client
        .post(format!("{GO}/api/v4/teams/{team_id}/members"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "team_id": team_id, "user_id": user_id }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "adding {user_id} to {team_id} failed: {}",
        response.text().await.unwrap_or_default()
    );
}

async fn create_channel_of_type(
    client: &reqwest::Client,
    admin_token: &str,
    team_id: &str,
    tag: &str,
    channel_type: &str,
) -> String {
    let response = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": format!("mmrs-parity-{tag}"),
            "display_name": format!("mmrs parity {tag}"),
            "type": channel_type,
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "creating the {channel_type} channel failed: {}",
        response.text().await.unwrap_or_default()
    );
    let created: serde_json::Value = response.json().await.expect("the channel decodes");
    created["id"].as_str().expect("an id").to_owned()
}

async fn login(client: &reqwest::Client, username: &str) -> String {
    let response = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username, "password": PASSWORD }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(response.status(), 200, "{username} cannot log in");
    response
        .headers()
        .get("token")
        .expect("Go returns a token header")
        .to_str()
        .expect("ASCII")
        .to_owned()
}

async fn build_fixture() -> Fixture {
    // Before anything is created: an aborted earlier run leaves rows whose usernames collide.
    purge_api_fixtures().await;

    let client = client();
    let admin_token = go_minted_token(&client).await;

    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin_token).await;
    let channel_id = create_channel_of_type(&client, &admin_token, &team_id, "autoc", "O").await;
    let private_channel_id =
        create_channel_of_type(&client, &admin_token, &team_id, "autocpriv", "P").await;

    // Usernames chosen so alphabetical order is `alpha` < `bravo` < `charlie`, and so that no
    // two share a searchable token beyond the prefix.
    let caller_id = create_user(
        &client,
        &admin_token,
        &format!("{PREFIX}alpha"),
        &format!("{PREFIX}alpha@mmrs.invalid"),
        "Autocfirst",
        "Autoclast",
        "Autocnick",
    )
    .await;
    let outsider_id = create_user(
        &client,
        &admin_token,
        &format!("{PREFIX}bravo"),
        &format!("{PREFIX}bravo@mmrs.invalid"),
        "Bravofirst",
        "Bravolast",
        "Bravonick",
    )
    .await;
    // The username carries the shared prefix; the *token* lives only in the address.
    let email_id = create_user(
        &client,
        &admin_token,
        &format!("{PREFIX}charlie"),
        &format!("{EMAIL_ONLY_TOKEN}@mmrs.invalid"),
        "Charliefirst",
        "Charlielast",
        "Charlienick",
    )
    .await;

    for id in [&caller_id, &outsider_id, &email_id] {
        add_to_team(&client, &admin_token, &team_id, id).await;
    }
    common::add_user_to_channel(&client, &admin_token, &channel_id, &caller_id).await;

    // The login is last and happens exactly once: it bumps `UpdateAt`, which is on the wire in
    // every response below. A login per test would move the row underneath a comparison.
    let caller_token = login(&client, &format!("{PREFIX}alpha")).await;

    Fixture {
        team_id,
        channel_id,
        private_channel_id,
        caller_id,
        caller_token,
        outsider_id,
        email_id,
        admin_token,
    }
}

fn skip() -> bool {
    !common::stack_enabled()
}

fn parse(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body).expect("the body is JSON")
}

/// Which top-level keys are **present**, alphabetically.
///
/// `serde_json` without `preserve_order` stores an object in a `BTreeMap`, so this cannot say
/// anything about wire order — and it does not need to: the byte comparison against Go pins the
/// order exactly, and `starts_with_users_key` pins the one place a reader might doubt it. What
/// this answers is the question `omitempty` raises, which is presence, not position.
fn keys(body: &[u8]) -> Vec<String> {
    parse(body)
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect()
}

/// `users` is the first field of `model.UserAutocomplete`, so `Encode` writes it first.
fn starts_with_users_key(body: &[u8]) {
    assert!(
        body.starts_with(br#"{"users":"#),
        "the document must open with `users`: {}",
        String::from_utf8_lossy(&body[..body.len().min(40)])
    );
}

fn usernames(body: &[u8], field: &str) -> Vec<String> {
    parse(body)[field]
        .as_array()
        .map(|users| {
            users
                .iter()
                .filter_map(|u| u["username"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The system-wide arm: no `in_team`, no `in_channel`, so `SearchUsersInTeam(rctx, "", …)`.
/// All three fixture users, in username order, byte for byte.
#[tokio::test]
async fn the_system_wide_arm_matches_go() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        &format!("/api/v4/users/autocomplete?name={PREFIX}"),
    )
    .await;
    assert_eq!(go, rs, "the system-wide arm diverges");

    assert_eq!(keys(&go), ["users"], "only `users` is ever populated here");
    assert_eq!(
        usernames(&go, "users"),
        [
            format!("{PREFIX}alpha"),
            format!("{PREFIX}bravo"),
            format!("{PREFIX}charlie")
        ],
        "ORDER BY Username ASC"
    );
}

/// `Encode` appends a newline where `Marshal` does not; the sibling `getUsers` handler eleven
/// lines away in the same Go file has none. One byte, and the byte comparisons above would
/// catch it — this names it so a failure says *what* broke.
#[tokio::test]
async fn the_body_ends_in_the_newline_encode_appends() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        "/api/v4/users/autocomplete?name=mmrsplainautocnosuchuser",
    )
    .await;
    assert_eq!(go, b"{\"users\":[]}\n", "Go's own bytes moved");
    assert_eq!(go, rs);
}

/// `in_team` only. `OutOfChannel` is never assigned on this arm, so `omitempty` drops it — the
/// key must be **absent**, not `null` and not `[]`.
#[tokio::test]
async fn the_in_team_arm_omits_out_of_channel_entirely() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        &format!(
            "/api/v4/users/autocomplete?in_team={}&name={PREFIX}",
            fx.team_id
        ),
    )
    .await;
    assert_eq!(go, rs, "the in-team arm diverges");

    assert_eq!(keys(&go), ["users"]);
    assert_eq!(
        usernames(&go, "users").len(),
        3,
        "all three are in the team"
    );
}

/// `in_channel` + `in_team`: both lists filled, and disjoint. This is the only arm that ever
/// puts `out_of_channel` on the wire.
#[tokio::test]
async fn the_in_channel_arm_fills_both_lists() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        &format!(
            "/api/v4/users/autocomplete?in_team={}&in_channel={}&name={PREFIX}",
            fx.team_id, fx.channel_id
        ),
    )
    .await;
    assert_eq!(go, rs, "the in-channel arm diverges");

    assert_eq!(keys(&go), ["out_of_channel", "users"], "both keys present");
    starts_with_users_key(&go);
    assert_eq!(usernames(&go, "users"), [format!("{PREFIX}alpha")]);
    assert_eq!(
        usernames(&go, "out_of_channel"),
        [format!("{PREFIX}bravo"), format!("{PREFIX}charlie")]
    );
}

/// The same arm with a term only the in-channel user matches: `out_of_channel` is an *empty*
/// slice on Go's side — non-nil, straight out of the store — and `omitempty` drops an empty
/// slice too. So the key vanishes, and `users` beside it stays `[]` because it has no
/// `omitempty` at all. The two rules, in one document, on live data.
#[tokio::test]
async fn an_empty_out_of_channel_list_is_an_absent_key() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        &format!(
            "/api/v4/users/autocomplete?in_team={}&in_channel={}&name={PREFIX}alpha",
            fx.team_id, fx.channel_id
        ),
    )
    .await;
    assert_eq!(go, rs);
    assert_eq!(keys(&go), ["users"], "out_of_channel is dropped when empty");
    assert_eq!(usernames(&go, "users"), [format!("{PREFIX}alpha")]);

    // And the mirror: a term only an out-of-channel user matches leaves `users` as `[]` while
    // `out_of_channel` is present. `users` never collapses, because it has no `omitempty`.
    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        &format!(
            "/api/v4/users/autocomplete?in_team={}&in_channel={}&name={PREFIX}bravo",
            fx.team_id, fx.channel_id
        ),
    )
    .await;
    assert_eq!(go, rs);
    assert_eq!(keys(&go), ["out_of_channel", "users"]);
    assert_eq!(parse(&go)["users"], serde_json::json!([]));
}

/// `Agents` is filled from the `mattermost-plugin-ai` bridge, which this deployment does not
/// have; the call errors, the field stays nil, and `omitempty` drops it. Asserted on **every
/// arm**, because a `Vec::new()` serialising as `[]` where Go emits nothing has been the
/// invisible wrong answer on this project twice.
#[tokio::test]
async fn agents_never_appears_on_any_arm() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    for path in [
        format!("/api/v4/users/autocomplete?name={PREFIX}"),
        format!(
            "/api/v4/users/autocomplete?in_team={}&name={PREFIX}",
            fx.team_id
        ),
        format!(
            "/api/v4/users/autocomplete?in_team={}&in_channel={}&name={PREFIX}",
            fx.team_id, fx.channel_id
        ),
    ] {
        let (go, rs) = fetch_both(&client, &fx.admin_token, &path).await;
        assert!(
            !keys(&go).iter().any(|k| k == "agents"),
            "{path}: Go grew an `agents` key — the bridge is reachable after all"
        );
        assert_eq!(go, rs, "{path}");
    }
}

/// `in_channel` with no `in_team` is a **500**, not a 400, and the id is the handler's own.
#[tokio::test]
async fn the_missing_team_id_error_matches_go() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &fx.admin_token,
        &format!(
            "/api/v4/users/autocomplete?in_channel={}&name={PREFIX}",
            fx.channel_id
        ),
    )
    .await;

    assert_eq!(go_status, 500, "Go calls this a server error, not a 400");
    assert_eq!(rs_status, go_status);
    let go_json =
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "missing_team_id");
    assert_eq!(
        go_json["id"],
        "api.user.autocomplete_users.missing_team_id.app_error"
    );
}

/// The `read_channel` gate is evaluated **before** the missing-team-id check, so an
/// unauthorised caller gets a 403 and never learns that the parameter combination was invalid.
/// Reversing the two would turn this into the 500 above — and leak the channel's existence.
#[tokio::test]
async fn the_channel_gate_runs_before_the_missing_team_check() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    // A private channel the caller is not in, and deliberately **no** `in_team`: the only thing
    // that keeps this from being the 500 above is the gate running first.
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &fx.caller_token,
        &format!(
            "/api/v4/users/autocomplete?in_channel={}&name={PREFIX}",
            fx.private_channel_id
        ),
    )
    .await;

    assert_eq!(go_status, 403, "the gate refuses before the check runs");
    assert_eq!(rs_status, go_status);
    let go_json = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "channel gate");
    assert_eq!(go_json["id"], "api.context.permissions.app_error");

    // The same caller on a channel it *can* read gets the ordinary answer, so the 403 above is
    // about the permission and not about the parameters.
    let (go, rs) = fetch_both(
        &client,
        &fx.caller_token,
        &format!(
            "/api/v4/users/autocomplete?in_team={}&in_channel={}&name={PREFIX}",
            fx.team_id, fx.channel_id
        ),
    )
    .await;
    assert_eq!(go, rs);
}

/// The `view_team` gate, with its own permission id. A team the caller does not belong to.
#[tokio::test]
async fn the_team_gate_matches_go() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    // A syntactically valid id for a team that does not exist — no team to create, no team to
    // tear down, and Go answers the same way it does for a team the caller merely is not in.
    let nowhere = "mmrsautocnoteam1111111111a";
    let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
        &client,
        &fx.caller_token,
        &format!("/api/v4/users/autocomplete?in_team={nowhere}&name={PREFIX}"),
    )
    .await;

    assert_eq!(go_status, 403, "a non-member is refused by `view_team`");
    assert_eq!(rs_status, go_status, "the team gate disagrees with Go");
    let go_json = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "team gate");
    assert_eq!(go_json["id"], "api.context.permissions.app_error");

    // The **admin** on the same nonexistent team is a 200 with an empty list: `manage_system`
    // clears the gate and the team join then matches nothing. So the 403 above belongs to the
    // permission, not to the team being missing — and this half fails if the gate is dropped.
    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        &format!("/api/v4/users/autocomplete?in_team={nowhere}&name={PREFIX}"),
    )
    .await;
    assert_eq!(go, rs);
    assert_eq!(go, b"{\"users\":[]}\n");
}

/// A multi-word `name` is several terms, and each is its own `AND` clause — so adding a word
/// **narrows** the result. This is also the only test that exercises the query-string decoding:
/// Go's `url.Query()` turns `+` into a space, and a parser that left it as a literal `+` would
/// search for one impossible term instead of two real ones.
#[tokio::test]
async fn a_multi_word_name_narrows_rather_than_widens() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    // Both encodings of the space, and both must behave the same.
    for separator in ["+", "%20"] {
        let (go, rs) = fetch_both(
            &client,
            &fx.admin_token,
            &format!("/api/v4/users/autocomplete?name={PREFIX}{separator}Autocfirst"),
        )
        .await;
        assert_eq!(go, rs, "separator {separator}");
        assert_eq!(
            usernames(&go, "users"),
            [format!("{PREFIX}alpha")],
            "{separator}: the second term is a first name only `alpha` has"
        );
    }

    // One word alone matches all three, so the narrowing above is real.
    let (go, _) = fetch_both(
        &client,
        &fx.admin_token,
        &format!("/api/v4/users/autocomplete?name={PREFIX}"),
    )
    .await;
    assert_eq!(usernames(&go, "users").len(), 3);

    // And a term nobody matches empties the result, which an OR of the clauses could not do.
    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        &format!("/api/v4/users/autocomplete?name={PREFIX}+nosuchtoken"),
    )
    .await;
    assert_eq!(go, rs);
    assert_eq!(go, b"{\"users\":[]}\n");
}

/// "Never autocomplete on emails" (api4/user.go:1399). The token lives only in one user's
/// address; searching it finds nobody — while that same user is findable by username, so the
/// empty answer is about the *column* and not about the row.
#[tokio::test]
async fn a_term_that_exists_only_in_an_email_matches_nobody() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        &format!("/api/v4/users/autocomplete?name={EMAIL_ONLY_TOKEN}"),
    )
    .await;
    assert_eq!(go, rs);
    assert_eq!(go, b"{\"users\":[]}\n", "an email term must find nobody");

    // The row exists and is reachable by name, and its address is on the wire.
    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        &format!("/api/v4/users/autocomplete?name={PREFIX}charlie"),
    )
    .await;
    assert_eq!(go, rs);
    assert_eq!(
        parse(&go)["users"][0]["email"],
        format!("{EMAIL_ONLY_TOKEN}@mmrs.invalid"),
        "the email field is returned; it is just not searched"
    );
}

/// The first and last names *are* searchable for this caller, because the fixture user holds
/// `manage_system` and that forces `AllowFullNames` on regardless of the setting.
#[tokio::test]
async fn the_name_columns_are_searchable_for_an_admin() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    for term in ["Autocfirst", "Autoclast", "Autocnick"] {
        let (go, rs) = fetch_both(
            &client,
            &fx.admin_token,
            &format!("/api/v4/users/autocomplete?name={term}"),
        )
        .await;
        assert_eq!(go, rs, "{term}");
        assert_eq!(
            usernames(&go, "users"),
            [format!("{PREFIX}alpha")],
            "{term} identifies exactly one fixture user"
        );
    }
}

/// The limit block, all four of its outcomes, against Go.
#[tokio::test]
async fn the_limit_block_matches_go() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    // A real limit truncates *after* the ordering.
    let (go, rs) = fetch_both(
        &client,
        &fx.admin_token,
        &format!("/api/v4/users/autocomplete?name={PREFIX}&limit=1"),
    )
    .await;
    assert_eq!(go, rs);
    assert_eq!(usernames(&go, "users"), [format!("{PREFIX}alpha")]);

    // Zero, and garbage-that-parses-to-zero, both return nothing — not the default of 100.
    for limit in ["0", "12abc", "1e3"] {
        let (go, rs) = fetch_both(
            &client,
            &fx.admin_token,
            &format!("/api/v4/users/autocomplete?name={PREFIX}&limit={limit}"),
        )
        .await;
        assert_eq!(go, rs, "limit={limit}");
        assert_eq!(go, b"{\"users\":[]}\n", "limit={limit} must return nothing");
    }

    // Above the ceiling, and a positive overflow that `Atoi` saturates to MaxInt: both clamp to
    // 1000 and answer exactly as the default does.
    let (default_go, _) = fetch_both(
        &client,
        &fx.admin_token,
        &format!("/api/v4/users/autocomplete?name={PREFIX}"),
    )
    .await;
    for limit in ["5000", "99999999999999999999"] {
        let (go, rs) = fetch_both(
            &client,
            &fx.admin_token,
            &format!("/api/v4/users/autocomplete?name={PREFIX}&limit={limit}"),
        )
        .await;
        assert_eq!(go, rs, "limit={limit}");
        assert_eq!(go, default_go, "limit={limit} clamps to the maximum");
    }
}

/// The clamp has no floor, so a negative limit reaches Postgres and fails the query — a 500
/// carrying the store's error id, on both servers. Adding a `max(0, …)` here would make this
/// server answer 200 where Go answers 500.
#[tokio::test]
async fn a_negative_limit_is_the_same_500_on_both_servers() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    for limit in ["-1", "-99999999999999999999"] {
        let ((go_status, go_body), (rs_status, rs_body)) = fetch_both_raw(
            &client,
            &fx.admin_token,
            &format!("/api/v4/users/autocomplete?name={PREFIX}&limit={limit}"),
        )
        .await;
        assert_eq!(go_status, 500, "limit={limit}");
        assert_eq!(rs_status, go_status, "limit={limit}");
        let go_json =
            assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "negative limit");
        assert_eq!(go_json["id"], "app.user.search.app_error");
    }
}

/// A non-admin caller sees a **differently sanitised** user than an admin does, and this route
/// has no self exception: the caller's own row goes through `SanitizeProfile` like everyone
/// else's. Compared byte for byte in both directions, so a port that swapped the two sanitizers
/// fails here rather than quietly leaking `auth_data`.
#[tokio::test]
async fn sanitisation_matches_go_for_both_an_admin_and_a_plain_caller() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    // The caller searching for *itself*: the row it has every reason to expect unsanitised.
    let self_path = format!("/api/v4/users/autocomplete?name={PREFIX}alpha");
    let (go_plain, rs_plain) = fetch_both(&client, &fx.caller_token, &self_path).await;
    assert_eq!(go_plain, rs_plain, "the non-admin view diverges");

    let (go_admin, rs_admin) = fetch_both(&client, &fx.admin_token, &self_path).await;
    assert_eq!(go_admin, rs_admin, "the admin view diverges");

    // And the two views are not the same document — otherwise this test would pass with the
    // sanitizer removed entirely.
    assert_ne!(
        go_plain, go_admin,
        "an admin and a plain caller must see different fields, or this proves nothing"
    );

    let plain = parse(&go_plain);
    let admin = parse(&go_admin);
    assert_eq!(plain["users"][0]["id"], fx.caller_id);

    // `ClearNonProfileFields(asAdmin)` is what separates the two views, and it separates them
    // in **both directions** — each view carries a key the other does not:
    //
    //   * `notify_props` is `map[string]string` with `omitempty`, so emptying it for a
    //     non-admin makes the key vanish; the admin keeps the populated map.
    //   * `auth_data` is a `*string` with `omitempty`, so `omitempty` drops it only when the
    //     pointer is **nil**. The non-admin path sets it to a pointer to `""`, which is not nil
    //     — so the key appears, as `""`, for the caller who is allowed to see *less*.
    //
    // A port that modelled `auth_data` as a plain `String` would drop it from both, and a port
    // that skipped the emptying would emit `notify_props` to everyone. Measured on Go's bytes.
    let plain_user = &plain["users"][0];
    let admin_user = &admin["users"][0];

    assert!(
        plain_user.get("notify_props").is_none(),
        "a non-admin's notify_props is emptied, and omitempty then drops the key"
    );
    assert!(
        admin_user
            .get("notify_props")
            .and_then(|v| v.as_object())
            .is_some_and(|m| !m.is_empty()),
        "the admin keeps the real map"
    );

    assert_eq!(
        plain_user.get("auth_data"),
        Some(&serde_json::Value::String(String::new())),
        "a pointer to the empty string survives omitempty as `\"\"`"
    );
    assert!(
        admin_user.get("auth_data").is_none(),
        "the admin's is still the nil pointer, which omitempty drops"
    );
}

/// The route registration itself: `autocomplete` is a literal sibling of `{user_id}`, and
/// registering it must not change what any other method or path does. A `POST` here was
/// forwarded before and must still be.
#[tokio::test]
async fn other_methods_on_the_path_are_still_forwarded() {
    if skip() {
        return;
    }
    let fx = fixture().await;
    let client = client();

    let response = client
        .post(format!("{RUST}/api/v4/users/autocomplete"))
        .header("Authorization", format!("Bearer {}", fx.admin_token))
        .send()
        .await
        .expect("the Rust server answers");
    assert_eq!(
        response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "a POST to this path must still reach Go, not our 405"
    );
}
