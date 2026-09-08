//! Cross-server parity for the session-activity pair: the write on the read path
//! (`UpdateLastActivityAtIfNeeded`) and the idle-timeout revoke that reads what it writes.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity session_activity
//! ```
//!
//! # Why this needs its own token per assertion
//!
//! Go caches sessions **by token** for `SessionCacheInMinutes` (platform/session.go:50), so a row
//! this suite edits behind Go's back is invisible to a Go server that has already seen that token.
//! Every assertion therefore plants a session with a **fresh, never-seen token**: Go's first
//! request with it is a guaranteed cache miss and reads the row we wrote. That is also why the
//! shared `go_minted_token` session is never used here — one of these tests deliberately gets a
//! session revoked, and revoking the suite-wide credential would take every other file down with
//! it.
//!
//! Planting a session row directly is the same trick the vertical slice was built on, in reverse:
//! there, a token Go minted authenticated against Rust; here, a row neither server minted
//! authenticates against both, which is only true because they share one `Sessions` table.
//!
//! # What is actually being compared
//!
//! Not response bodies — the response is identical whether or not the write happened. The oracle
//! is the **database column**, read before and after a request to each server in turn. Four
//! questions, each asked of both servers:
//!
//! 1. Does a stale session get its `LastActivityAt` refreshed by `GET /users/me`?
//! 2. Does a recently-refreshed one get left alone (the five-minute throttle)?
//! 3. Does `GET /users/username/{name}` refresh it? (Go: no. That asymmetry is the whole reason
//!    `ActivityUpdate` exists as a parameter rather than a line in the shared helper.)
//! 4. Does a session idle past `SessionIdleTimeoutInMinutes` get refused *and deleted*?

use crate::common;

use common::{GO, RUST, client, go_minted_token, logged_in_user_id, stack_enabled};
use sqlx::PgPool;
use std::time::Duration;

/// Go's default `SessionIdleTimeoutInMinutes`, which the live document also carries.
const IDLE_TIMEOUT_MINUTES: i64 = 43_200;
/// `model.SessionActivityTimeout` — the five-minute write throttle.
const ACTIVITY_TIMEOUT_MILLIS: i64 = 1000 * 60 * 5;

const MINUTE: i64 = 60_000;

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://mmuser:mmuser_password@localhost:5432/mattermost".into());
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("connects to the shared Postgres")
}

/// `model.GetMillis`, borrowed from `mm-model` so the test clock and the server clock are the
/// same function.
fn now_millis() -> i64 {
    mm_model::utils::get_millis()
}

/// Plant a session for the logged-in fixture user, idle by `idle_millis`, and return its token.
///
/// The id and token are 26 characters in this repo's `mmrs` namespace with a per-call suffix, so
/// two assertions never collide and a human reading `Sessions` can see where the row came from.
/// `ExpiresAt = 0` means "never expires" on both servers (`IsExpired` returns false at `<= 0`),
/// which keeps expiry out of the way of the thing under test.
async fn plant_session(pool: &PgPool, tag: &str, idle_millis: i64) -> String {
    let id = format!("mmrssessactv{tag:0>14}");
    let token = format!("mmrssessactt{tag:0>14}");
    assert_eq!(id.len(), 26, "session ids are 26 characters");
    assert_eq!(token.len(), 26, "and so are tokens");

    sqlx::query("DELETE FROM sessions WHERE id = $1 OR token = $2")
        .bind(&id)
        .bind(&token)
        .execute(pool)
        .await
        .expect("clears any leftover from a failed run");

    sqlx::query(
        "INSERT INTO sessions
             (id, token, createat, expiresat, lastactivityat, userid, deviceid, roles,
              isoauth, props, expirednotify, voipdeviceid)
         VALUES ($1, $2, $3, 0, $3, $4, '', 'system_user', false, '{}'::jsonb, false, '')",
    )
    .bind(&id)
    .bind(&token)
    .bind(now_millis() - idle_millis)
    .bind(logged_in_user_id())
    .execute(pool)
    .await
    .expect("plants the session row");

    token
}

async fn activity_of(pool: &PgPool, token: &str) -> Option<i64> {
    sqlx::query_scalar::<_, Option<i64>>("SELECT lastactivityat FROM sessions WHERE token = $1")
        .bind(token)
        .fetch_optional(pool)
        .await
        .expect("reads the column")
        .flatten()
}

/// Remove exactly the rows this test planted, by token.
///
/// Deliberately **not** a `LIKE 'mmrssessactv%'` sweep. The parity tests share one binary and run
/// concurrently, so a prefix purge at the end of one test deletes the sessions another is midway
/// through asserting on — measured, and it presented as `LastActivityAt` reading back as `None`
/// rather than as anything that looked like a cleanup problem. Every tag in this file is unique
/// for the same reason.
async fn purge(pool: &PgPool, tokens: &[&str]) {
    for token in tokens {
        sqlx::query("DELETE FROM sessions WHERE token = $1")
            .bind(token)
            .execute(pool)
            .await
            .expect("purges the planted session row");
    }
}

/// One authenticated GET, returning `(status, x-mmrs-served-by)`.
async fn get(base: &str, token: &str, path: &str) -> (u16, Option<String>) {
    let response = client()
        .get(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the server is reachable");
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (response.status().as_u16(), served_by)
}

/// The shared login has to have happened before anything here runs: [`logged_in_user_id`] is what
/// every planted row hangs off, and it is only set as a side effect of minting the token.
async fn fixture_user_id() {
    let _ = go_minted_token(&client()).await;
}

/// **The write.** A session idle past the five-minute throttle is refreshed by `GET /users/me` on
/// both servers.
///
/// This is [D-084] closed, measured rather than argued: before this change the Rust column stayed
/// exactly where the fixture put it while Go's moved, so a user whose traffic this server answered
/// drifted towards an idle-timeout revoke performed by the process next door.
#[tokio::test]
async fn both_servers_refresh_a_stale_session_on_users_me() {
    if !stack_enabled() {
        return;
    }
    fixture_user_id().await;
    let pool = pool().await;

    let mut planted = Vec::new();
    for (tag, base, expect_served_by) in
        [("gorefresh", GO, None), ("rsrefresh", RUST, Some("rust"))]
    {
        let seeded_idle = ACTIVITY_TIMEOUT_MILLIS + 5 * MINUTE;
        let token = plant_session(&pool, tag, seeded_idle).await;
        planted.push(token.clone());
        let before = activity_of(&pool, &token)
            .await
            .expect("the row is planted");

        let (status, served_by) = get(base, &token, "/api/v4/users/me").await;
        assert_eq!(status, 200, "{base} accepted the planted session");
        if let Some(expected) = expect_served_by {
            assert_eq!(
                served_by.as_deref(),
                Some(expected),
                "/users/me was forwarded, so this proves nothing about the Rust handler"
            );
        }

        let after = activity_of(&pool, &token)
            .await
            .expect("the session survived");
        assert!(
            after > before,
            "{base} did not refresh LastActivityAt: {before} -> {after}"
        );
        assert!(
            after >= now_millis() - 30_000,
            "{base} wrote a value that is not roughly now: {after}"
        );
    }

    purge(
        &pool,
        &planted.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
}

/// **The throttle.** A session refreshed less than five minutes ago is left exactly alone — the
/// column does not move by so much as a millisecond, on either server.
///
/// Go's guard is `now - LastActivityAt < SessionActivityTimeout`. Inverting it would turn every
/// authenticated read into a write; asserting *equality* rather than "not much later" is what
/// makes that visible.
#[tokio::test]
async fn neither_server_writes_inside_the_five_minute_throttle() {
    if !stack_enabled() {
        return;
    }
    fixture_user_id().await;
    let pool = pool().await;

    let mut planted = Vec::new();
    for (tag, base) in [("gothrottl", GO), ("rsthrottl", RUST)] {
        let token = plant_session(&pool, tag, MINUTE).await;
        planted.push(token.clone());
        let before = activity_of(&pool, &token)
            .await
            .expect("the row is planted");

        let (status, _) = get(base, &token, "/api/v4/users/me").await;
        assert_eq!(status, 200);

        assert_eq!(
            activity_of(&pool, &token).await,
            Some(before),
            "{base} wrote LastActivityAt inside the throttle window"
        );
    }

    purge(
        &pool,
        &planted.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
}

/// **A 304 is not activity.** Go calls `UpdateLastActivityAtIfNeeded` *after* `HandleEtag` has
/// returned, so a conditional request that hits the cache leaves the session clock alone on both
/// servers.
///
/// This pins the call's **position** inside the handler rather than its presence, which is the
/// half a reader is most likely to lose when tidying: moved three lines up, it would be correct on
/// every 200 and wrong on every 304, and no body comparison would ever see it.
#[tokio::test]
async fn a_304_is_not_activity_on_either_server() {
    if !stack_enabled() {
        return;
    }
    fixture_user_id().await;
    let pool = pool().await;

    let mut planted = Vec::new();
    for (tag, base) in [("goetag304", GO), ("rsetag304", RUST)] {
        // A first request, inside the throttle window, to learn the etag without moving the clock.
        let token = plant_session(&pool, tag, MINUTE).await;
        planted.push(token.clone());
        let first = client()
            .get(format!("{base}/api/v4/users/me"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the server is reachable");
        assert_eq!(first.status().as_u16(), 200);
        let etag = first
            .headers()
            .get("etag")
            .expect("getUser sets an etag")
            .to_str()
            .expect("the etag is ASCII")
            .to_owned();

        // Now age the session past the throttle and repeat the request conditionally.
        let stale = now_millis() - (ACTIVITY_TIMEOUT_MILLIS + 5 * MINUTE);
        sqlx::query("UPDATE sessions SET lastactivityat = $1 WHERE token = $2")
            .bind(stale)
            .bind(&token)
            .execute(&pool)
            .await
            .expect("ages the session");

        // **Retry on a moved etag.** `/users/me`'s etag folds in `Users.UpdateAt`, and the admin
        // row is shared with every other suite in this binary — one of them touching it between
        // the two requests turns the conditional read into a 200 and the assertion below fails
        // about the wrong thing. The session's activity is *not* shared (it is planted here), so
        // re-reading the etag and repeating is sound: what is under test is the 304's effect, not
        // which etag produced it.
        let mut etag = etag;
        let mut second_status = 0;
        for attempt in 1..=6 {
            // Re-plant the aged activity each time: a 200 on a previous attempt moved it.
            sqlx::query("UPDATE sessions SET lastactivityat = $1 WHERE token = $2")
                .bind(stale)
                .bind(&token)
                .execute(&pool)
                .await
                .expect("ages the session");

            let second = client()
                .get(format!("{base}/api/v4/users/me"))
                .header("Authorization", format!("Bearer {token}"))
                .header("If-None-Match", &etag)
                .send()
                .await
                .expect("the server is reachable");
            second_status = second.status().as_u16();
            if second_status == 304 {
                break;
            }
            // The row moved: take the etag this answer carries and try once more.
            if let Some(fresh) = second
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
            {
                etag = fresh;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50 * attempt)).await;
        }
        assert_eq!(second_status, 304, "{base} did not answer 304");

        assert_eq!(
            activity_of(&pool, &token).await,
            Some(stale),
            "{base} counted a 304 as activity"
        );
    }

    purge(
        &pool,
        &planted.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
}

/// **The second call site.** `getUsers` refreshes the clock too (api4/user.go:1169), and it is a
/// different handler with a different tail — so it is asserted separately rather than assumed to
/// follow `getUser`.
#[tokio::test]
async fn both_servers_refresh_a_stale_session_on_the_users_page() {
    if !stack_enabled() {
        return;
    }
    fixture_user_id().await;
    let pool = pool().await;

    let mut planted = Vec::new();
    for (tag, base, expect_served_by) in
        [("gouserpag", GO, None), ("rsuserpag", RUST, Some("rust"))]
    {
        let token = plant_session(&pool, tag, ACTIVITY_TIMEOUT_MILLIS + 5 * MINUTE).await;
        planted.push(token.clone());
        let before = activity_of(&pool, &token)
            .await
            .expect("the row is planted");

        let (status, served_by) = get(base, &token, "/api/v4/users?page=0&per_page=1").await;
        assert_eq!(status, 200, "{base} served the users page");
        if let Some(expected) = expect_served_by {
            assert_eq!(
                served_by.as_deref(),
                Some(expected),
                "/api/v4/users was forwarded, so this proves nothing about the Rust handler"
            );
        }

        let after = activity_of(&pool, &token)
            .await
            .expect("the session survived");
        assert!(
            after > before,
            "{base} did not refresh LastActivityAt from the users page: {before} -> {after}"
        );
    }

    purge(
        &pool,
        &planted.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
}

/// **The route split.** `getUserByUsername` does not touch the session clock on either server,
/// though it is otherwise the same handler as `getUser`.
///
/// Go calls `UpdateLastActivityAtIfNeeded` at api4/user.go:352 — inside `getUser` — and in exactly
/// three other places, none of them this handler. Sharing the tail between the two routes here
/// makes it very easy to share the write too; this is the assertion that would catch it, and it
/// checks the claim against Go rather than against the reading of a Go file.
#[tokio::test]
async fn get_user_by_username_refreshes_nothing_on_either_server() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin_token = go_minted_token(&client).await;
    let username = common::username_of(&client, &admin_token, logged_in_user_id()).await;
    let pool = pool().await;

    let mut planted = Vec::new();
    for (tag, base) in [("gobyuname", GO), ("rsbyuname", RUST)] {
        let token = plant_session(&pool, tag, ACTIVITY_TIMEOUT_MILLIS + 5 * MINUTE).await;
        planted.push(token.clone());
        let before = activity_of(&pool, &token)
            .await
            .expect("the row is planted");

        let (status, _) = get(base, &token, &format!("/api/v4/users/username/{username}")).await;
        assert_eq!(status, 200, "{base} served the username route");

        assert_eq!(
            activity_of(&pool, &token).await,
            Some(before),
            "{base} refreshed the session on a route Go leaves alone"
        );
    }

    purge(
        &pool,
        &planted.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
}

/// **The revoke.** A session idle past `SessionIdleTimeoutInMinutes` is refused with a 401 *and*
/// its row is deleted — on both servers.
///
/// This is [D-088]. The deletion is the half that matters: a 401 alone would leave the row for the
/// next request to be refused by again, and Go's contract is that the credential stops existing.
///
/// Go performs the delete in a goroutine it does not wait for, so the row's disappearance is
/// polled there; ours is synchronous and gone by the time the 401 arrives. Neither ordering is
/// visible to a client, which is why this port took the synchronous one.
#[tokio::test]
async fn an_idle_session_is_refused_and_revoked_by_both_servers() {
    if !stack_enabled() {
        return;
    }
    fixture_user_id().await;
    let pool = pool().await;

    let idle = (IDLE_TIMEOUT_MINUTES + 60) * MINUTE;
    for (tag, base) in [("goidleout", GO), ("rsidleout", RUST)] {
        let token = plant_session(&pool, tag, idle).await;
        assert!(
            activity_of(&pool, &token).await.is_some(),
            "the row is planted"
        );

        let (status, _) = get(base, &token, "/api/v4/users/me").await;
        assert_eq!(
            status, 401,
            "{base} accepted a session idle past the timeout"
        );

        // Go revokes asynchronously; poll rather than sleep, so the Rust half — which is already
        // done — costs one query.
        let mut gone = false;
        for _ in 0..40 {
            if activity_of(&pool, &token).await.is_none() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(gone, "{base} refused the session but left the row behind");
    }
    // No purge: the assertion *is* that both servers deleted these rows.
}

/// The refusal body matches Go's, field for field, and carries no hint that the session was
/// revoked rather than never valid.
///
/// # This is the test that found a pre-existing divergence on every migrated route
///
/// `App::GetSession` builds `api.context.invalid_token.error`, and it never reaches a client:
/// `handlers.go:277-280` keeps a 500 and replaces every other failure with the generic
/// `api.context.session_expired.app_error`. This server returned the inner id — so a wrong token
/// produced one id from Go and a different one from us, on every route that takes a session, and
/// clients switch on exactly that string. Nothing had compared a 401 *body* before; the idle
/// timeout needed one, and the comparison failed on its first run.
///
/// The indistinguishability is the point on Go's side: a bad token, an expired session, a session
/// id used as a token and a session revoked for idleness all answer the same way, which tells a
/// caller nothing about which credentials exist.
#[tokio::test]
async fn the_idle_refusal_body_matches_go() {
    if !stack_enabled() {
        return;
    }
    fixture_user_id().await;
    let pool = pool().await;
    let idle = (IDLE_TIMEOUT_MINUTES + 60) * MINUTE;

    let mut bodies = Vec::new();
    let mut planted = Vec::new();
    for (tag, base) in [("gobodyidl", GO), ("rsbodyidl", RUST)] {
        let token = plant_session(&pool, tag, idle).await;
        planted.push(token.clone());
        let response = client()
            .get(format!("{base}/api/v4/users/me"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("the server is reachable");
        assert_eq!(response.status().as_u16(), 401);
        bodies.push(response.bytes().await.expect("a body").to_vec());
    }

    // `message` (D-092, untranslated) and `request_id` are the only fields allowed to differ.
    let go = common::assert_error_bodies_match_except_known_gaps(
        &bodies[0],
        &bodies[1],
        "the idle-timeout refusal",
    );
    assert_eq!(
        go["id"], "api.context.session_expired.app_error",
        "the id is the one the web layer substitutes, not the one GetSession built"
    );

    purge(
        &pool,
        &planted.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
}

/// The same substitution for a token that was never a session, which is the far more common way
/// to reach that branch — and the shape the divergence above was actually living in.
#[tokio::test]
async fn an_unknown_token_gets_the_same_refusal_as_an_idle_one() {
    if !stack_enabled() {
        return;
    }
    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let response = client()
            .get(format!("{base}/api/v4/users/me"))
            .header("Authorization", "Bearer mmrsnosuchtokennosuchtok01")
            .send()
            .await
            .expect("the server is reachable");
        assert_eq!(response.status().as_u16(), 401);
        bodies.push(response.bytes().await.expect("a body").to_vec());
    }

    let go = common::assert_error_bodies_match_except_known_gaps(
        &bodies[0],
        &bodies[1],
        "an unknown token",
    );
    assert_eq!(go["id"], "api.context.session_expired.app_error");
}

/// The `Set-Cookie` Go writes when it rejects a token, on every location a token can arrive from.
///
/// [D-169], closed. `handlers.go:278` calls `RemoveSessionCookie` immediately before substituting
/// the generic error id, so the two halves of that branch were found together and are asserted
/// together. The header is compared verbatim rather than parsed: `Max-Age=0` (Go maps `MaxAge:
/// -1` onto the literal zero) and the attribute order are both things a re-rendering could get
/// subtly wrong.
///
/// # The `Path` here can only ever be `/`, and that is worth saying
///
/// `RemoveSessionCookie` scopes the cookie to `GetSubpathFromConfig(SiteURL)`. On this stack the
/// document holds `SiteURL = ""` and the Go container runs on `http://localhost:8065` — and both
/// have an **empty URL path**, so both produce `/`. The two servers therefore agree here for a
/// reason that has nothing to do with the port being right, and a real subpath deployment cannot
/// be produced without reconfiguring the container. The subpath logic itself is pinned against Go
/// by `fixtures/behaviour_subpath.json` instead; this test covers the rest of the branch.
#[tokio::test]
async fn a_rejected_token_clears_the_session_cookie_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    const BAD: &str = "mmrsnosuchtokennosuchtok02";
    const EXPECTED: &str = "MMAUTHTOKEN=; Path=/; Max-Age=0; HttpOnly";

    for base in [GO, RUST] {
        // Every location `parse_auth_token` accepts. Go clears the cookie for all of them: the
        // branch turns on the token being *present and rejected*, not on where it came from.
        let requests = [
            (
                "bearer",
                client()
                    .get(format!("{base}/api/v4/users/me"))
                    .header("Authorization", format!("Bearer {BAD}")),
            ),
            (
                "cookie",
                client()
                    .get(format!("{base}/api/v4/users/me"))
                    .header("Cookie", format!("MMAUTHTOKEN={BAD}")),
            ),
            (
                "query",
                client().get(format!("{base}/api/v4/users/me?access_token={BAD}")),
            ),
        ];

        for (location, request) in requests {
            let response = request.send().await.expect("the server is reachable");
            assert_eq!(response.status().as_u16(), 401, "{base} via {location}");
            let cookies: Vec<&str> = response
                .headers()
                .get_all("set-cookie")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .collect();
            assert_eq!(
                cookies,
                vec![EXPECTED],
                "{base} via {location} did not clear the session cookie"
            );
        }
    }
}

/// …and it is **not** cleared when no token was presented at all.
///
/// Go only enters that branch when `token != ""` (handlers.go:270); a request with no credential
/// gets its 401 from `ApiSessionRequired`'s `TokenRequired` arm, which does not touch the cookie.
/// Nothing to clear is not the same as something to clear, and a port that cleared unconditionally
/// would look correct on every test above.
#[tokio::test]
async fn a_request_with_no_token_gets_no_cookie_on_either_server() {
    if !stack_enabled() {
        return;
    }
    for base in [GO, RUST] {
        let response = client()
            .get(format!("{base}/api/v4/users/me"))
            .send()
            .await
            .expect("the server is reachable");
        assert_eq!(response.status().as_u16(), 401);
        assert_eq!(
            response.headers().get_all("set-cookie").iter().count(),
            0,
            "{base} cleared a cookie for a request that carried none"
        );
    }
}
