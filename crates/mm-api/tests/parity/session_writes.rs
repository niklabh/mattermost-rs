//! Cross-server parity for the session **write** family.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity_session_writes
//! ```
//!
//! ```text
//! POST /api/v4/users/{user_id}/sessions/revoke      revokeSession
//! POST /api/v4/users/{user_id}/sessions/revoke/all  revokeAllSessionsForUser
//! POST /api/v4/users/sessions/revoke/all            revokeAllSessionsAllUsers
//! PUT  /api/v4/users/sessions/device                handleDeviceProps
//! ```
//!
//! # Every one of these routes destroys the credential it is called with
//!
//! So the shape of this suite is not the usual "send the same request to both servers and diff
//! the bytes". Two rules, applied throughout:
//!
//! - **Refusals are compared on both servers.** A 400 or a 403 changes nothing, so the same
//!   request goes to Go and to us and the bodies are diffed. These branches are the substance of
//!   the family — who may revoke whose session, and which of five error ids comes back — and they
//!   are all here.
//! - **Successes get one throwaway session each.** A revoke that works cannot be replayed, so
//!   each server is given its own freshly minted session of its own freshly created user, and the
//!   two answers are compared to each other. Nothing in this file ever revokes the admin token
//!   the rest of the binary is using.
//!
//! # What is deliberately not exercised
//!
//! `POST /users/sessions/revoke/all` succeeds by deleting **every session row on the server**,
//! including those of every test running concurrently in this binary. Its 403 branch is asserted
//! against both servers below; its 200 branch is left to
//! `mm_app::session::tests::the_all_users_revoke_removes_access_data_first`, which pins the thing
//! that actually matters about it — that access data is deleted before sessions. Running the real
//! thing here would make every other suite in this binary flaky for a branch a unit test already
//! covers. See [D-351].

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, create_plain_user,
    delete_plain_user, go_minted_token, login_plain_user, post_both_raw, purge_api_fixtures,
    stack_enabled,
};

/// `{"status":"OK"}` — `ReturnStatusOK`'s whole body, with no trailing newline.
const STATUS_OK: &[u8] = br#"{"status":"OK"}"#;

/// PUT raw bytes to both servers, the counterpart of [`post_both_raw`] for the one route in this
/// family that is not a POST.
///
/// Local to this file rather than added to `common`: it is the only PUT-with-raw-bytes caller in
/// the binary, and a shared module is the thing two concurrent worktrees collide on.
async fn put_both_raw(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let put = async |base: &str| {
        let response = client
            .put(format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        (
            response.status().as_u16(),
            response.bytes().await.expect("body reads").to_vec(),
        )
    };

    (put(GO).await, put(RUST).await)
}

/// Send one request to one server and return `(status, body, set_cookie)`.
async fn put_one(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, Vec<u8>, Option<String>) {
    let response = client
        .put(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let cookie = response
        .headers()
        .get("set-cookie")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("body reads").to_vec(),
        cookie,
    )
}

/// POST to one server, returning `(status, body)`.
async fn post_one(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, Vec<u8>) {
    let response = client
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    (
        response.status().as_u16(),
        response.bytes().await.expect("body reads").to_vec(),
    )
}

/// Every session id a user currently has, through Go.
///
/// `Session::Sanitize` clears the token before the list is serialised, so there is **no way to
/// map a token to its session id** from this endpoint — which is why the callers below identify a
/// new session by set difference across a login rather than by reading it off the token.
async fn session_ids(client: &reqwest::Client, admin_token: &str, user_id: &str) -> Vec<String> {
    let response = client
        .get(format!("{GO}/api/v4/users/{user_id}/sessions"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    let sessions: serde_json::Value = response.json().await.expect("the list decodes");
    sessions
        .as_array()
        .expect("an array")
        .iter()
        .filter_map(|session| session["id"].as_str().map(str::to_owned))
        .collect()
}

/// Log `tag` in again and return `(token, session_id)` for the session that login created.
///
/// The id is the one that was not there a moment ago. Taking `sessions[0]` instead returns
/// whichever row the query happened to order first — the same id for every login of the same
/// user, which is how the first version of this suite "revoked" one session twice.
async fn extra_login(
    client: &reqwest::Client,
    admin_token: &str,
    user_id: &str,
    tag: &str,
) -> (String, String) {
    let before = session_ids(client, admin_token, user_id).await;
    let token = login_plain_user(client, tag).await;
    let after = session_ids(client, admin_token, user_id).await;
    let new: Vec<String> = after
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect();
    assert_eq!(
        new.len(),
        1,
        "a login must create exactly one session for {tag}"
    );
    (token, new[0].clone())
}

/// How many live sessions a user has, through Go.
async fn session_count(client: &reqwest::Client, admin_token: &str, user_id: &str) -> usize {
    let response = client
        .get(format!("{GO}/api/v4/users/{user_id}/sessions"))
        .header("Authorization", format!("Bearer {admin_token}"))
        .send()
        .await
        .expect("Go answers");
    let sessions: serde_json::Value = response.json().await.expect("the list decodes");
    sessions.as_array().map_or(0, Vec::len)
}

/// Log the suite's admin in again, returning a second token for the same user.
async fn login_as_admin(client: &reqwest::Client) -> String {
    let login = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({
            "login_id": common::LOGIN_ID,
            "password": common::PASSWORD,
        }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(login.status(), 200, "the admin cannot log in");
    login
        .headers()
        .get("token")
        .expect("Go returns a token header")
        .to_str()
        .expect("ASCII")
        .to_owned()
}

/// The user id a token belongs to.
async fn user_id_of(client: &reqwest::Client, token: &str) -> String {
    let response = client
        .get(format!("{GO}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let user: serde_json::Value = response.json().await.expect("the user decodes");
    user["id"].as_str().expect("an id").to_owned()
}

/// True when the token still authenticates — asked of **this** server, not of Go.
///
/// Go must not be asked, and the reason is a measured divergence rather than a preference: the Go
/// server caches sessions in memory and only invalidates that cache from its own revocation
/// paths, so a session row this server deletes keeps authenticating against Go until the entry
/// ages out. Measured 2026-09-12 — delete the row by hand and `GET {go}/users/me` still answers
/// 200 while `GET {rust}/users/me` answers 401. See [D-350]; `go_cache_keeps_a_session_we_revoked`
/// below pins it deliberately so it cannot be mistaken for a flake here.
async fn token_still_works(client: &reqwest::Client, token: &str) -> bool {
    client
        .get(format!("{RUST}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("our server answers")
        .status()
        .is_success()
}

/// The same question asked of Go, for the one test that is about the difference.
async fn token_still_works_against_go(client: &reqwest::Client, token: &str) -> bool {
    client
        .get(format!("{GO}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .status()
        .is_success()
}

// ------------------------------------------------------------------------------------------
// revokeSession
// ------------------------------------------------------------------------------------------

/// Every refusal branch of `revokeSession`, compared byte for byte against Go.
///
/// The five are not variations on one error: they carry **four different ids** across two status
/// codes, and two of them name a parameter the client did not get wrong. Taken in the order the
/// handler reaches them, which is itself part of what is being asserted — a body with no
/// `session_id` and a malformed `{user_id}` must produce the *url param* error, because the path
/// is validated first.
#[tokio::test]
async fn every_refusal_branch_of_revoke_session_matches_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &token).await;
    let target = create_plain_user(&client, &token, &team_id, "revsessa").await;
    let other = create_plain_user(&client, &token, &team_id, "revsessb").await;
    let other_session = session_ids(&client, &token, &other.id)
        .await
        .pop()
        .expect("the other user logged in, so it has a session");

    let me = format!("/api/v4/users/{}/sessions/revoke", target.id);

    // 1. A malformed `{user_id}`: `RequireUserId` runs before anything else, so the body is never
    //    read and a perfectly good `session_id` does not save it.
    //
    //    `shortid`, not `not-an-id`. A hyphen is outside Go's mux charset for this segment, so
    //    `mux_segments_or_forward` hands the request to Go for Go to 404 it — a correct answer
    //    that proves nothing about this handler. The id has to be *mux-valid and Require-invalid*
    //    to reach the branch under test, which means letters and digits of the wrong length.
    let (go, rs) = post_both_raw(
        &client,
        &token,
        "/api/v4/users/shortid/sessions/revoke",
        br#"{"session_id":"whatever"}"#,
    )
    .await;
    assert_eq!(go.0, rs.0, "a bad user id: status");
    assert_eq!(go.0, 400);
    let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, "a bad user id");
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");

    // 2. An absent `session_id`, 3. an empty one, and 4. a body that is not JSON at all — all the
    //    same 400 on the same parameter, because `MapFromJSON` swallows every failure into an
    //    empty map and the handler only tests the value for emptiness.
    for (label, body) in [
        ("an empty body", &b"{}"[..]),
        ("an empty session_id", br#"{"session_id":""}"#),
        ("a non-JSON body", b"not json at all"),
        ("a JSON array", b"[]"),
        ("a non-string session_id", br#"{"session_id":5}"#),
    ] {
        let (go, rs) = post_both_raw(&client, &token, &me, body).await;
        assert_eq!(go.0, rs.0, "{label}: status");
        assert_eq!(go.0, 400, "{label}");
        let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, label);
        assert_eq!(
            body["id"], "api.context.invalid_body_param.app_error",
            "{label}"
        );
    }

    // 5. A `session_id` that names nothing: the session is **fetched** before it is checked to
    //    belong to `{user_id}`, so this is the store's error and not an ownership one.
    let (go, rs) = post_both_raw(
        &client,
        &token,
        &me,
        br#"{"session_id":"nosuchsessionnosuchsess01"}"#,
    )
    .await;
    assert_eq!(go.0, rs.0, "an unknown session: status");
    assert_eq!(go.0, 400);
    let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, "an unknown session");
    assert_eq!(body["id"], "app.session.get.app_error");

    // 6. A session that exists but belongs to somebody else: 400, and the parameter named is
    //    **`user_id`** rather than the `session_id` the caller actually got wrong. That
    //    difference from branch 5 is what lets a caller with `edit_other_users` tell a live
    //    session id from a dead one, and it is Go's.
    let (go, rs) = post_both_raw(
        &client,
        &token,
        &me,
        format!(r#"{{"session_id":"{other_session}"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(go.0, rs.0, "somebody else's session: status");
    assert_eq!(go.0, 400);
    let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, "somebody else's session");
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");

    // 6b. The ownership check compares the session's owner against the **path** user, not
    //     against the caller. Those two are the same value in every case above, so this is the
    //     only shape that tells them apart: the admin names `target`'s path but hands in a
    //     session of the admin's own. `session.UserId != c.Params.UserId` is true — admin is not
    //     target — so it is a 400 and nothing is revoked. Comparing against the caller instead
    //     would make it false and **revoke the admin's own session** through another user's URL.
    //
    //     A second admin login is used rather than the suite's shared token, so that a run
    //     against broken code destroys a session nothing else depends on.
    let spare_admin = login_as_admin(&client).await;
    let admin_id = user_id_of(&client, &token).await;
    let spare_admin_session = session_ids(&client, &token, &admin_id)
        .await
        .into_iter()
        .next()
        .expect("the admin has sessions");
    let (go, rs) = post_both_raw(
        &client,
        &token,
        &me,
        format!(r#"{{"session_id":"{spare_admin_session}"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(
        go.0, rs.0,
        "the admin's own session on another user's path: status"
    );
    assert_eq!(go.0, 400);
    let body = assert_error_bodies_match_except_known_gaps(
        &go.1,
        &rs.1,
        "the admin's own session on another user's path",
    );
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");
    assert!(
        token_still_works(&client, &spare_admin).await,
        "the admin's own session must not be revocable through another user's path"
    );

    // 7. A plain user aiming at somebody else: 403 naming `edit_other_users`, before the body is
    //    read — so a valid `session_id` does not change the answer and the caller learns nothing
    //    about whether the target exists.
    let path_to_other = format!("/api/v4/users/{}/sessions/revoke", other.id);
    let (go, rs) = post_both_raw(
        &client,
        &target.token,
        &path_to_other,
        format!(r#"{{"session_id":"{other_session}"}}"#).as_bytes(),
    )
    .await;
    assert_eq!(go.0, rs.0, "a plain user aiming elsewhere: status");
    assert_eq!(go.0, 403);
    let body =
        assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, "a plain user aiming elsewhere");
    assert_eq!(body["id"], "api.context.permissions.app_error");

    // And the refusal is identical for a user id that is well-formed but names nobody — the gate
    // runs before any lookup, so "you may not" and "there is no such user" are one answer.
    let nobody = "abcdefghijklmnopqrstuvwxyz";
    let (go_nobody, rs_nobody) = post_both_raw(
        &client,
        &target.token,
        &format!("/api/v4/users/{nobody}/sessions/revoke"),
        br#"{"session_id":"whatever"}"#,
    )
    .await;
    assert_eq!(go_nobody.0, rs_nobody.0);
    assert_eq!(go_nobody.0, 403);
    assert_error_bodies_match_except_known_gaps(&go_nobody.1, &rs_nobody.1, "a nonexistent user");

    // Nothing above was destructive: both users still have their sessions.
    assert!(token_still_works(&client, &target.token).await);
    assert!(token_still_works(&client, &other.token).await);

    delete_plain_user(&client, &token, &target.id).await;
    delete_plain_user(&client, &token, &other.id).await;
}

/// The success branch, one throwaway session per server.
///
/// A plain user revoking **its own** session passes `SessionHasPermissionToUserOrBot` on the self
/// branch, which is the case a real client hits from the "active sessions" screen. Each server
/// gets its own extra login so neither replays the other's work, and the surviving session is the
/// one the call was made with.
#[tokio::test]
async fn revoking_a_session_matches_go_and_only_kills_that_session() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "revsessok").await;

    // Two extra logins, so there are three sessions: the one the call is made with, plus one
    // sacrificial session for each server.
    let (doomed_by_go, go_victim) = extra_login(&client, &admin, &user.id, "revsessok").await;
    let (doomed_by_rs, rs_victim) = extra_login(&client, &admin, &user.id, "revsessok").await;
    assert_ne!(go_victim, rs_victim, "two logins, two sessions");
    assert_eq!(session_count(&client, &admin, &user.id).await, 3);

    let path = format!("/api/v4/users/{}/sessions/revoke", user.id);
    let go = post_one(
        &client,
        GO,
        &user.token,
        &path,
        &serde_json::json!({ "session_id": go_victim }),
    )
    .await;
    let rs = post_one(
        &client,
        RUST,
        &user.token,
        &path,
        &serde_json::json!({ "session_id": rs_victim }),
    )
    .await;

    assert_eq!(go.0, 200, "Go revoked its victim");
    assert_eq!(rs.0, go.0, "same status");
    assert_eq!(rs.1, go.1, "same body");
    assert_eq!(rs.1, STATUS_OK, "ReturnStatusOK, no trailing newline");

    // Both victims are gone, the caller's own session is not, and nothing else was touched.
    assert!(!token_still_works(&client, &doomed_by_go).await);
    assert!(!token_still_works(&client, &doomed_by_rs).await);
    assert!(token_still_works(&client, &user.token).await);
    assert_eq!(
        session_count(&client, &admin, &user.id).await,
        1,
        "exactly the two victims were revoked"
    );

    delete_plain_user(&client, &admin, &user.id).await;
}

// ------------------------------------------------------------------------------------------
// revokeAllSessionsForUser
// ------------------------------------------------------------------------------------------

/// The refusal branch: a plain user aiming at somebody else is 403 naming `edit_other_users`,
/// and a malformed `{user_id}` is 400 on the url param. Both compared against Go.
#[tokio::test]
async fn revoke_all_for_user_refuses_the_same_way_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let one = create_plain_user(&client, &admin, &team_id, "revallref").await;
    let two = create_plain_user(&client, &admin, &team_id, "revallrefb").await;

    let (go, rs) = post_both_raw(
        &client,
        &one.token,
        &format!("/api/v4/users/{}/sessions/revoke/all", two.id),
        b"",
    )
    .await;
    assert_eq!(go.0, rs.0, "a plain user aiming elsewhere: status");
    assert_eq!(go.0, 403);
    let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, "revoke/all, refused");
    assert_eq!(body["id"], "api.context.permissions.app_error");

    // Mux-valid, `RequireUserId`-invalid — see the note in the revoke suite above.
    let (go, rs) = post_both_raw(
        &client,
        &admin,
        "/api/v4/users/shortid/sessions/revoke/all",
        b"",
    )
    .await;
    assert_eq!(go.0, rs.0, "a bad user id: status");
    assert_eq!(go.0, 400);
    let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, "revoke/all, bad id");
    assert_eq!(body["id"], "api.context.invalid_url_param.app_error");

    // Refusals are not destructive.
    assert!(token_still_works(&client, &one.token).await);
    assert!(token_still_works(&client, &two.token).await);

    delete_plain_user(&client, &admin, &one.id).await;
    delete_plain_user(&client, &admin, &two.id).await;
}

/// The success branch, one throwaway user per server, so neither replays the other's revocation.
///
/// This route takes the **whole** list, so the token the call is made with dies too — asserted,
/// because it is the behaviour a caller is most likely to have guarded against by accident.
#[tokio::test]
async fn revoke_all_for_user_kills_every_session_including_the_callers() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let by_go = create_plain_user(&client, &admin, &team_id, "revallgo").await;
    let by_rs = create_plain_user(&client, &admin, &team_id, "revallrs").await;

    // Two sessions each, so "all" is more than one row and a port that revoked only the caller's
    // own would still fail.
    let go_extra = login_plain_user(&client, "revallgo").await;
    let rs_extra = login_plain_user(&client, "revallrs").await;
    assert_eq!(session_count(&client, &admin, &by_go.id).await, 2);
    assert_eq!(session_count(&client, &admin, &by_rs.id).await, 2);

    let go = post_one(
        &client,
        GO,
        &by_go.token,
        &format!("/api/v4/users/{}/sessions/revoke/all", by_go.id),
        &serde_json::json!({}),
    )
    .await;
    let rs = post_one(
        &client,
        RUST,
        &by_rs.token,
        &format!("/api/v4/users/{}/sessions/revoke/all", by_rs.id),
        &serde_json::json!({}),
    )
    .await;

    assert_eq!(go.0, 200);
    assert_eq!(rs.0, go.0, "same status");
    assert_eq!(rs.1, go.1, "same body");
    assert_eq!(rs.1, STATUS_OK);

    assert_eq!(session_count(&client, &admin, &by_go.id).await, 0);
    assert_eq!(
        session_count(&client, &admin, &by_rs.id).await,
        0,
        "we must revoke the whole list, not just the caller's own session"
    );
    assert!(!token_still_works(&client, &by_rs.token).await);
    assert!(!token_still_works(&client, &rs_extra).await);
    assert!(!token_still_works(&client, &go_extra).await);

    delete_plain_user(&client, &admin, &by_go.id).await;
    delete_plain_user(&client, &admin, &by_rs.id).await;
}

// ------------------------------------------------------------------------------------------
// revokeAllSessionsAllUsers
// ------------------------------------------------------------------------------------------

/// The gate on the all-users revoke is **`manage_system`**, not `edit_other_users`, and it has no
/// self branch.
///
/// So a plain user is refused here even though the same user succeeds against
/// `/users/me/sessions/revoke/all` — the two routes look alike and are not alike, and reusing the
/// other gate would hand every user a button that logs out the whole server. The 403's `details`
/// names the permission, which is why the body is diffed rather than only the status.
///
/// The 200 branch is not exercised; see this module's header and [D-351].
#[tokio::test]
async fn the_all_users_revoke_is_manage_system_and_refuses_a_plain_user() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "revallall").await;

    let (go, rs) = post_both_raw(
        &client,
        &plain.token,
        "/api/v4/users/sessions/revoke/all",
        b"",
    )
    .await;
    assert_eq!(go.0, rs.0, "status");
    assert_eq!(go.0, 403);
    let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, "all-users revoke");
    assert_eq!(body["id"], "api.context.permissions.app_error");
    // The permission itself is **not on the wire**: Go builds `userId=…, permission=manage_system`
    // into `DetailedError`, which the handler suppresses outside debug mode, so both servers send
    // the same `detailed_error: ""`. The discriminator that *is* observable is the pair of
    // answers below — refused here, allowed on the per-user route — and that is what separates
    // `manage_system` from `edit_other_users` from outside.
    assert_eq!(body["detailed_error"], "");

    // The same user succeeds against its own `/revoke/all`, which is the whole point of the
    // distinction — asserted last, because it destroys the token.
    let own = post_one(
        &client,
        RUST,
        &plain.token,
        &format!("/api/v4/users/{}/sessions/revoke/all", plain.id),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(
        own.0, 200,
        "the per-user route is `edit_other_users` and passes on the self branch"
    );

    // Still nobody else's problem: the admin token is untouched.
    assert!(token_still_works(&client, &admin).await);

    delete_plain_user(&client, &admin, &plain.id).await;
}

// ------------------------------------------------------------------------------------------
// handleDeviceProps
// ------------------------------------------------------------------------------------------

/// Every validation branch of `PUT /users/sessions/device`, in the order the handler reaches
/// them.
///
/// All four parameters are 400 `api.context.invalid_body_param.app_error` differing **only** in
/// which parameter they name, so the status alone proves nothing here. The last case is the one
/// that pins the *order*: a request with all four wrong must name
/// `device_notification_disabled`, because that is the first thing validated — not `device_id`,
/// which is what a reader who validated the ids first would produce.
#[tokio::test]
async fn every_validation_branch_of_the_device_route_matches_go() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "devprops").await;

    let path = "/api/v4/users/sessions/device";
    let cases: [(&str, &[u8]); 10] = [
        // `device_notification_disabled` is an exact match against two literals.
        (
            "device_notification_disabled",
            br#"{"device_notification_disabled":"True"}"#,
        ),
        (
            "device_notification_disabled",
            br#"{"device_notification_disabled":"1"}"#,
        ),
        (
            "device_notification_disabled",
            br#"{"device_notification_disabled":"yes"}"#,
        ),
        // `mobile_version` is strict semver: no `v` prefix, three parts, no leading zeroes.
        ("mobile_version", br#"{"mobile_version":"v2.34.0"}"#),
        ("mobile_version", br#"{"mobile_version":"2.34"}"#),
        ("mobile_version", br#"{"mobile_version":"01.2.3"}"#),
        ("mobile_version", br#"{"mobile_version":"2.34.0-"}"#),
        // The device ids, each against its own allowlist.
        ("device_id", br#"{"device_id":"nonsense"}"#),
        ("device_id", br#"{"device_id":"apple_rn:"}"#),
        // Android is a valid *standard* platform and not a valid VoIP one, which is the whole
        // difference between the two allowlists.
        (
            "voip_device_id",
            br#"{"voip_device_id":"android_rn:tokentoken"}"#,
        ),
    ];

    for (parameter, body) in cases {
        let (go, rs) = put_both_raw(&client, &user.token, path, body).await;
        let label = format!("{parameter}: {}", String::from_utf8_lossy(body));
        assert_eq!(go.0, rs.0, "{label}: status");
        assert_eq!(go.0, 400, "{label}");
        let decoded = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, &label);
        assert_eq!(
            decoded["id"], "api.context.invalid_body_param.app_error",
            "{label}"
        );
        // The parameter name lives in the interpolated message, which is what makes these ten
        // cases four distinct answers rather than one.
        let message = decoded["message"].as_str().unwrap_or_default().to_owned()
            + decoded["detailed_error"].as_str().unwrap_or_default();
        assert!(
            message.contains(parameter)
                || decoded["message"] == "Invalid or missing parameter in request body.",
            "{label}: the body should name {parameter}: {decoded}"
        );
    }

    // The order: everything wrong at once must be refused on the **first** thing validated.
    let all_wrong = br#"{"device_notification_disabled":"maybe","mobile_version":"v1","device_id":"nonsense","voip_device_id":"nonsense"}"#;
    let (go, rs) = put_both_raw(&client, &user.token, path, all_wrong).await;
    assert_eq!(go.0, rs.0);
    assert_eq!(go.0, 400);
    let decoded =
        assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, "everything wrong at once");
    assert_eq!(decoded["id"], "api.context.invalid_body_param.app_error");

    // None of that was destructive.
    assert!(token_still_works(&client, &user.token).await);

    delete_plain_user(&client, &admin, &user.id).await;
}

/// An empty body is a **success**: nothing to validate, no device id, no props to write.
///
/// Reachable — a client that sends `{}` gets a 200 and the server writes nothing — and it is the
/// branch where the `changed` short-circuit in `SetExtraSessionProps` is the only thing between
/// a no-op call and a database write on every request.
#[tokio::test]
async fn an_empty_device_body_is_a_no_op_success() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "devnoop").await;

    for body in [&b"{}"[..], b"", b"not json"] {
        let (go, rs) =
            put_both_raw(&client, &user.token, "/api/v4/users/sessions/device", body).await;
        let label = String::from_utf8_lossy(body).into_owned();
        assert_eq!(go.0, rs.0, "{label}: status");
        assert_eq!(go.0, 200, "{label}");
        assert_eq!(rs.1, go.1, "{label}: body");
        assert_eq!(rs.1, STATUS_OK, "{label}");
        // No cookie: `attachDeviceIds` was never entered.
        assert!(
            token_still_works(&client, &user.token).await,
            "{label}: nothing was revoked"
        );
    }

    delete_plain_user(&client, &admin, &user.id).await;
}

/// The props half: a valid `mobile_version` and `device_notification_disabled` are written onto
/// the session and are visible through `GET /users/me/sessions`.
///
/// One user per server, because the second call on the same session would take the *unchanged*
/// branch and write nothing — which would make a broken write indistinguishable from a working
/// one.
#[tokio::test]
async fn the_device_route_writes_session_props_the_way_go_does() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let by_go = create_plain_user(&client, &admin, &team_id, "devpropgo").await;
    let by_rs = create_plain_user(&client, &admin, &team_id, "devproprs").await;

    let body = serde_json::json!({
        "mobile_version": "2.34.0",
        "device_notification_disabled": "true",
    });
    let path = "/api/v4/users/sessions/device";
    let go = put_one(&client, GO, &by_go.token, path, &body).await;
    let rs = put_one(&client, RUST, &by_rs.token, path, &body).await;

    assert_eq!(go.0, 200);
    assert_eq!(rs.0, go.0, "same status");
    assert_eq!(rs.1, go.1, "same body");
    assert_eq!(rs.1, STATUS_OK);
    // No device id was sent, so `attachDeviceIds` never ran and neither server sets a cookie.
    assert_eq!(go.2, None, "Go sets no cookie without a device id");
    assert_eq!(rs.2, None, "and neither do we");

    let props_of = async |token: &str| -> serde_json::Value {
        let response = client
            .get(format!("{GO}/api/v4/users/me/sessions"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        let sessions: serde_json::Value = response.json().await.expect("decodes");
        sessions.as_array().expect("an array")[0]["props"].clone()
    };

    let go_props = props_of(&by_go.token).await;
    let rs_props = props_of(&by_rs.token).await;
    for props in [&go_props, &rs_props] {
        assert_eq!(props["mobile_version"], "2.34.0", "{props}");
        assert_eq!(props["device_notification_disabled"], "true", "{props}");
        // The CSRF prop the login planted must survive — `SetExtraSessionProps` merges, it does
        // not replace the object.
        assert!(
            props.get("csrf").is_some(),
            "the existing props must survive the merge: {props}"
        );
    }

    delete_plain_user(&client, &admin, &by_go.id).await;
    delete_plain_user(&client, &admin, &by_rs.id).await;
}

/// The device-id half: the cookie, the expiry rewrite, and the revocation of other sessions
/// holding the same device id.
///
/// Three things are asserted that a port can get individually wrong: the `Set-Cookie` header's
/// attributes match Go's, the *other* session on the same device is gone while the caller's own
/// survives, and the caller's `expires_at` moved.
#[tokio::test]
async fn attaching_a_device_id_matches_go_on_the_cookie_and_the_revocation() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let by_go = create_plain_user(&client, &admin, &team_id, "devidgo").await;
    let by_rs = create_plain_user(&client, &admin, &team_id, "devidrs").await;

    let device = "apple_rn:mmrsdevicetokenforparity";
    let path = "/api/v4/users/sessions/device";
    let body = serde_json::json!({ "device_id": device });

    // Give each user a second session already holding the same device id, so there is something
    // for the revocation half to find. The second session registers the device first; the first
    // session then registers it and must kill the second.
    let go_stale = login_plain_user(&client, "devidgo").await;
    let rs_stale = login_plain_user(&client, "devidrs").await;
    assert_eq!(put_one(&client, GO, &go_stale, path, &body).await.0, 200);
    assert_eq!(put_one(&client, RUST, &rs_stale, path, &body).await.0, 200);
    assert_eq!(session_count(&client, &admin, &by_go.id).await, 2);
    assert_eq!(session_count(&client, &admin, &by_rs.id).await, 2);

    let go = put_one(&client, GO, &by_go.token, path, &body).await;
    let rs = put_one(&client, RUST, &by_rs.token, path, &body).await;

    assert_eq!(go.0, 200);
    assert_eq!(rs.0, go.0, "same status");
    assert_eq!(rs.1, go.1, "same body");

    // The cookie. `Expires` is an absolute instant a second apart between the two calls, so it is
    // compared by shape rather than by value; everything else is compared verbatim.
    let go_cookie =
        go.2.expect("Go sets a session cookie when a device id is attached");
    let rs_cookie = rs.2.expect("and so must we");
    let strip_expires = |cookie: &str| -> Vec<String> {
        cookie
            .split("; ")
            .filter(|part| !part.starts_with("Expires="))
            .map(|part| {
                // The token differs between the two users; compare the attribute, not the value.
                if part.starts_with("MMAUTHTOKEN=") {
                    "MMAUTHTOKEN=<token>".to_owned()
                } else {
                    part.to_owned()
                }
            })
            .collect()
    };
    assert_eq!(
        strip_expires(&rs_cookie),
        strip_expires(&go_cookie),
        "cookie attributes must match\n  go: {go_cookie}\n  rs: {rs_cookie}"
    );
    assert!(
        go_cookie.contains("Expires=") && rs_cookie.contains("Expires="),
        "both must carry an absolute Expires as well as Max-Age"
    );

    // The revocation: the stale session on the same device is gone, the caller's own is not.
    assert!(!token_still_works(&client, &go_stale).await);
    assert!(
        !token_still_works(&client, &rs_stale).await,
        "the other session holding this device id must be revoked"
    );
    assert!(
        token_still_works(&client, &by_rs.token).await,
        "and the caller's own must not be — `session.Id != currentSessionId`"
    );
    assert_eq!(session_count(&client, &admin, &by_rs.id).await, 1);
    assert_eq!(session_count(&client, &admin, &by_go.id).await, 1);

    // The device id and the new expiry are on the row.
    let session_of = async |token: &str| -> serde_json::Value {
        let response = client
            .get(format!("{GO}/api/v4/users/me/sessions"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        let sessions: serde_json::Value = response.json().await.expect("decodes");
        sessions.as_array().expect("an array")[0].clone()
    };
    let go_session = session_of(&by_go.token).await;
    let rs_session = session_of(&by_rs.token).await;
    assert_eq!(rs_session["device_id"], device);
    assert_eq!(go_session["device_id"], device);
    // `UpdateDeviceId` writes `ExpiredNotify = false` alongside the new expiry — the fourth
    // column in that statement, and the one a reader drops because the handler never mentions
    // it. It is `false` here either way, which is exactly why it must be asserted: the only
    // thing that could make it `true` is writing the wrong constant.
    assert_eq!(rs_session["expired_notify"], false);
    assert_eq!(rs_session["expired_notify"], go_session["expired_notify"]);
    // Both expiries were rewritten from the same config value against sessions created seconds
    // apart, so they agree to within a small window rather than exactly.
    let go_expiry = go_session["expires_at"].as_i64().expect("an expiry");
    let rs_expiry = rs_session["expires_at"].as_i64().expect("an expiry");
    assert!(
        (go_expiry - rs_expiry).abs() < 60_000,
        "the mobile expiry must be computed the same way: go={go_expiry} rs={rs_expiry}"
    );

    delete_plain_user(&client, &admin, &by_go.id).await;
    delete_plain_user(&client, &admin, &by_rs.id).await;
}

// ------------------------------------------------------------------------------------------
// The strangler's own divergence
// ------------------------------------------------------------------------------------------

/// A session **we** revoke keeps authenticating against the **Go** server.
///
/// This is not a wire-format difference and no single-server test can see it. Go keeps sessions in
/// an in-memory cache (`platform.PlatformService`, `SessionCacheSize = 35000`) and invalidates it
/// only from its own revocation paths — `ClearUserSessionCache`, which also fans out over the
/// cluster bus. This server has no such cache ([D-087]) and no way to reach Go's, so every
/// revocation in this family leaves Go serving the dead session until its entry ages out.
///
/// It is asserted rather than merely recorded because the failure mode is silent and the
/// direction matters: a **security** control that appears to work from the client that issued it.
/// If a future change makes Go agree — a shared cache, a cluster message, a shorter TTL — this
/// test fails and [D-350] can be closed, which is exactly what should happen.
#[tokio::test]
async fn go_cache_keeps_a_session_we_revoked() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let user = create_plain_user(&client, &admin, &team_id, "gocache").await;

    // Warm Go's cache for this token, so the entry definitely exists before the row goes.
    assert!(token_still_works_against_go(&client, &user.token).await);

    let revoked = post_one(
        &client,
        RUST,
        &user.token,
        &format!("/api/v4/users/{}/sessions/revoke/all", user.id),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(revoked.0, 200);

    // The row is gone — this is a store read, not a cache read.
    assert_eq!(session_count(&client, &admin, &user.id).await, 0);
    // We refuse it.
    assert!(!token_still_works(&client, &user.token).await);
    // Go does not. If this assertion starts failing, the gap has closed; see the doc above.
    assert!(
        token_still_works_against_go(&client, &user.token).await,
        "D-350 appears to be fixed — Go now rejects a session we revoked. Close the entry and \
         invert this assertion rather than deleting it."
    );

    delete_plain_user(&client, &admin, &user.id).await;
}

/// Updating **one** device column must not wipe the other.
///
/// `UpdateDeviceId` always writes both, so `attachDeviceIds` reads the current session and passes
/// the existing value back for whichever id the caller did not send — Go added that fallback
/// deliberately and says so in a comment (user.go:2809). Without it, a phone that registers its
/// push token and then its VoIP token ends up with only the second, and the first request's work
/// is silently undone by the second.
///
/// Asserted in both directions, because the fallback is two separate `if`s and dropping either
/// one is its own bug.
#[tokio::test]
async fn attaching_one_device_id_does_not_wipe_the_other() {
    if !stack_enabled() {
        eprintln!("skipping: set MM_PARITY_STACK=1 with the stack running");
        return;
    }
    purge_api_fixtures().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = common::a_team_and_channel_the_user_is_in(&client, &admin).await;
    let by_go = create_plain_user(&client, &admin, &team_id, "devkeepgo").await;
    let by_rs = create_plain_user(&client, &admin, &team_id, "devkeeprs").await;

    let standard = "apple_rn:mmrsstandarddevicetoken";
    let voip = "apple_rn:mmrsvoipdevicetoken";
    let path = "/api/v4/users/sessions/device";

    let session_of = async |token: &str| -> serde_json::Value {
        let response = client
            .get(format!("{GO}/api/v4/users/me/sessions"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("Go answers");
        let sessions: serde_json::Value = response.json().await.expect("decodes");
        sessions.as_array().expect("an array")[0].clone()
    };

    for (base, token, label) in [(GO, &by_go.token, "go"), (RUST, &by_rs.token, "rs")] {
        // VoIP first, then the standard id on its own.
        let first = put_one(
            &client,
            base,
            token,
            path,
            &serde_json::json!({ "voip_device_id": voip }),
        )
        .await;
        assert_eq!(first.0, 200, "{label}: the voip attach");
        let second = put_one(
            &client,
            base,
            token,
            path,
            &serde_json::json!({ "device_id": standard }),
        )
        .await;
        assert_eq!(second.0, 200, "{label}: the standard attach");

        let session = session_of(token).await;
        assert_eq!(session["device_id"], standard, "{label}");
        assert_eq!(
            session["voip_device_id"], voip,
            "{label}: the voip id set by the first call must survive the second"
        );
    }

    // And the other order, on fresh sessions, so a fallback that only works one way is caught.
    let (go_again, _) = extra_login(&client, &admin, &by_go.id, "devkeepgo").await;
    let (rs_again, _) = extra_login(&client, &admin, &by_rs.id, "devkeeprs").await;
    for (base, token, label) in [(GO, &go_again, "go"), (RUST, &rs_again, "rs")] {
        assert_eq!(
            put_one(
                &client,
                base,
                token,
                path,
                &serde_json::json!({ "device_id": standard }),
            )
            .await
            .0,
            200,
            "{label}"
        );
        assert_eq!(
            put_one(
                &client,
                base,
                token,
                path,
                &serde_json::json!({ "voip_device_id": voip }),
            )
            .await
            .0,
            200,
            "{label}"
        );
        let session = session_of(token).await;
        assert_eq!(
            session["device_id"], standard,
            "{label}: the standard id must survive a voip-only update"
        );
        assert_eq!(session["voip_device_id"], voip, "{label}");
    }

    delete_plain_user(&client, &admin, &by_go.id).await;
    delete_plain_user(&client, &admin, &by_rs.id).await;
}
