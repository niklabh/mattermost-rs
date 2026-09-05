//! Cross-server parity for the `me` alias on every served route with a `{user_id}` segment.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity me_alias
//! ```
//!
//! # Why this is a suite and not a line in each route's own tests
//!
//! `RequireUserId` (web/context.go:296) substitutes the session's user id for the literal `me`
//! **before** calling `IsValidId`. Every api4 route with a `{user_id}` segment therefore accepts
//! it, and the webapp prefers it to the real id on most reads. A port that validates first is not
//! stricter, it is wrong: 400 where Go answers 200.
//!
//! Four served routes did exactly that — `/users/me/channel_members`, `/users/me/posts/flagged`,
//! `/users/me/teams/{team}/threads` and `.../threads/{thread}` — and none of their own suites
//! could see it, because each tests the route with an explicit id. The bug is in the *class*, so
//! the test is too: [`every_served_user_route_accepts_me`] walks the whole list. A new route with
//! a `{user_id}` segment belongs in that list.
//!
//! # Passing validation is not the same as resolving correctly
//!
//! An alias resolved to the wrong user still answers 200. [`me_is_the_session_user_not_just_valid`]
//! reads two routes twice — once as `me`, once as the caller's own id — and requires the bytes to
//! match, which a substitution of anyone else's id would fail.

use crate::common;

use common::{
    RUST, add_user_to_channel, client, create_channel_typed, create_team, fetch_both_raw,
    go_minted_token, logged_in_user_id, post_message, stack_enabled,
};

struct Fixture {
    team_id: String,
    channel_id: String,
    thread_id: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team_id = create_team(client, token, "mealias").await;
            let channel_id = create_channel_typed(client, token, &team_id, "mealias", "O").await;
            add_user_to_channel(client, token, &channel_id, logged_in_user_id()).await;

            // A real thread, so the single-thread route answers 200 rather than 404 and the
            // sweep compares a served body instead of two error pages.
            let thread_id = post_message(client, token, &channel_id, "me alias root", None).await;
            post_message(client, token, &channel_id, "reply", Some(&thread_id)).await;

            Fixture {
                team_id,
                channel_id,
                thread_id,
            }
        })
        .await
}

/// Every served route with a `{user_id}` segment, addressed as `me`.
fn paths(f: &Fixture) -> Vec<String> {
    let (team, channel, thread) = (&f.team_id, &f.channel_id, &f.thread_id);
    [
        "/api/v4/users/me".to_owned(),
        "/api/v4/users/me/status".to_owned(),
        "/api/v4/users/me/sessions".to_owned(),
        "/api/v4/users/me/preferences".to_owned(),
        "/api/v4/users/me/preferences/display".to_owned(),
        "/api/v4/users/me/preferences/display/name/use_military_time".to_owned(),
        "/api/v4/users/me/terms_of_service".to_owned(),
        "/api/v4/users/me/teams".to_owned(),
        "/api/v4/users/me/teams/unread".to_owned(),
        "/api/v4/users/me/channels".to_owned(),
        "/api/v4/users/me/channel_members?per_page=2".to_owned(),
        "/api/v4/users/me/posts/flagged?per_page=2".to_owned(),
        format!("/api/v4/users/me/teams/{team}/unread"),
        format!("/api/v4/users/me/teams/{team}/channels"),
        format!("/api/v4/users/me/teams/{team}/channels/members"),
        format!("/api/v4/users/me/teams/{team}/channels/categories"),
        format!("/api/v4/users/me/channels/{channel}/unread"),
        format!("/api/v4/users/me/channels/{channel}/posts/unread"),
        format!("/api/v4/users/me/teams/{team}/drafts"),
        format!("/api/v4/users/me/teams/{team}/threads?per_page=2"),
        format!("/api/v4/users/me/teams/{team}/threads/{thread}"),
    ]
    .to_vec()
}

/// Both servers answer the same status for `me` on every one of them, and we serve it ourselves.
///
/// `fetch_both_raw` asserts the `x-mmrs-served-by: rust` header, so a route that quietly forwarded
/// `me` to Go rather than resolving it would fail here too — which is the other way to make this
/// test pass without fixing anything.
#[tokio::test]
async fn every_served_user_route_accepts_me() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut mismatched = Vec::new();
    for path in paths(f) {
        let ((go_status, _), (rs_status, _)) = fetch_both_raw(&client, &token, &path).await;
        if go_status != rs_status {
            mismatched.push(format!("{path}: go {go_status}, rust {rs_status}"));
        }
    }
    assert!(
        mismatched.is_empty(),
        "`me` must resolve before validation on every route:\n  {}",
        mismatched.join("\n  ")
    );
}

/// And it resolves to the **session's** user, not merely to something valid.
#[tokio::test]
async fn me_is_the_session_user_not_just_valid() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let id = logged_in_user_id();

    // Two of the four routes the sweep found. Both carry the caller's id in the answer, so a
    // substitution of anyone else's would show up in the bytes rather than only in the status.
    for (aliased, explicit) in [
        (
            format!(
                "/api/v4/users/me/teams/{}/threads/{}",
                f.team_id, f.thread_id
            ),
            format!(
                "/api/v4/users/{id}/teams/{}/threads/{}",
                f.team_id, f.thread_id
            ),
        ),
        (
            "/api/v4/users/me/channel_members?per_page=2".to_owned(),
            format!("/api/v4/users/{id}/channel_members?per_page=2"),
        ),
    ] {
        let one = read_rust(&client, &token, &aliased).await;
        let two = read_rust(&client, &token, &explicit).await;
        assert_eq!(
            String::from_utf8_lossy(&one),
            String::from_utf8_lossy(&two),
            "{aliased} and {explicit} must be the same read"
        );
    }
}

async fn read_rust(client: &reqwest::Client, token: &str, path: &str) -> Vec<u8> {
    let response = client
        .get(format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{path} is unreachable: {e}"));
    assert_eq!(response.status().as_u16(), 200, "{path}");
    response.bytes().await.expect("body reads").to_vec()
}
