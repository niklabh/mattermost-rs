//! Cross-server parity for account creation and the two e-mail-token routes.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity user_creates
//! ```
//!
//! # Every account this file creates is destroyed by the test that created it
//!
//! `GET /api/v4/users/stats` counts rows, and `parity/users_stats.rs` compares that count across
//! two servers. A user left behind here is an off-by-one there, in a suite that has nothing to do
//! with this one — it has happened, to a different fixture. So every create is followed by a
//! **hard** delete through [`scrub`], not Go's soft `DELETE /users/{id}`, which only sets
//! `DeleteAt` and would still be visible to anything counting rows in `Users`.
//!
//! The prefix is `mmrsnewuser`, distinct from `create_plain_user`'s `mmrsplain`, so the sweep in
//! `common::purge_api_fixtures` can tell an abandoned account of this suite's from one of theirs.
//!
//! # Two servers, two accounts, and what that lets a test assert
//!
//! A create cannot be issued to both bases with the same body: the second would collide on the
//! username. So each comparison mints a *pair* of accounts with different tags and compares the
//! two responses field by field, exempting `id`, `username`, `email` and the timestamps. That is
//! weaker than the byte comparison the read routes use, and it is the strongest thing available
//! — a route whose whole purpose is to allocate an id cannot produce two identical bodies.
//!
//! The **refusals** are not subject to that and are compared as bodies, which is where most of
//! the assertions here live.
//!
//! # The stack has `EnableOpenServer=true`
//!
//! Set as an environment override on both servers by `scripts/go-server.sh` and
//! `scripts/mm-api-env.sh`. So the anonymous branch of `createUser` *succeeds* here rather than
//! answering `api.user.create_user.no_open_server`, and the `SanitizeInput(false)` clears are
//! observable. A stack with the flag off would exercise the 403 instead; both are covered, the
//! second by pointing the assertion at the flag rather than at a literal.

use crate::common;

use common::{
    GO, PLAIN_USER_PASSWORD, RUST, assert_error_bodies_match_except_known_gaps, client,
    go_minted_token, stack_enabled,
};

/// The shared fixture pool — capped acquire timeout, one connection, [`None`] without a
/// `DATABASE_URL`.
async fn pool() -> Option<sqlx::PgPool> {
    common::fixture_pool().await
}

/// The username this suite derives from a tag. Lower-case and alphanumeric, because
/// `IsValidUsername` is stricter than the e-mail rule beside it.
fn new_username(tag: &str) -> String {
    format!("mmrsnewuser{tag}")
}

fn new_email(tag: &str) -> String {
    format!("{}@mmrs.invalid", new_username(tag))
}

/// Remove every trace of an account this suite created.
///
/// **Hard, not soft.** Go's `DELETE /users/{id}` sets `DeleteAt` and leaves the row, its username
/// and its e-mail reserved — so the next run of this file could not create the same account, and
/// `users_stats` would still see the row in its `Count(*)` for the options that include deleted
/// users. The three tables are everything a freshly created user owns: `createUserOrGuest` writes
/// `Users` and three `Preferences`, and a login (which nothing here performs) would add
/// `Sessions`.
async fn scrub(tag: &str) {
    let Some(pool) = pool().await else {
        return;
    };
    let username = new_username(tag);
    for statement in [
        "DELETE FROM preferences WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM sessions WHERE userid IN (SELECT id FROM users WHERE username = $1)",
        "DELETE FROM users WHERE username = $1",
    ] {
        let _ = sqlx::query(statement).bind(&username).execute(&pool).await;
    }
}

/// How many `Tokens` rows name `email` in their `Extra` blob.
///
/// **Scoped, deliberately.** A global `COUNT(*)` over `Tokens` is shared state and the parity
/// tests run concurrently: the first draft of this file counted the whole table and the two
/// "mints exactly one token" tests counted each other's rows, so a green run and a red one
/// differed only in scheduling. Both token types encode the address in `Extra`
/// (`{"UserId":…,"Email":…}` for recovery, the same pair for verification), so a `LIKE` on the
/// address is an exact, per-test handle.
async fn tokens_naming(email: &str) -> i64 {
    let Some(pool) = pool().await else {
        return -1;
    };
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM tokens WHERE extra LIKE $1")
        .bind(format!("%{email}%"))
        .fetch_one(&pool)
        .await
        .unwrap_or(-1)
}

/// `POST` a body to one base, optionally authenticated, and return the status and the parsed
/// body.
async fn post(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: Option<&str>,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let mut request = http
        .post(format!("{base}{path}"))
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).expect("a body"));
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.expect("the server answers");
    let status = response.status().as_u16();
    let bytes = response.bytes().await.expect("a body");
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

/// The same, returning raw bytes so an error body can be compared key by key.
async fn post_raw(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    token: Option<&str>,
    body: &[u8],
) -> (u16, Vec<u8>) {
    let mut request = http
        .post(format!("{base}{path}"))
        .header("Content-Type", "application/json")
        .body(body.to_vec());
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request.send().await.expect("the server answers");
    let status = response.status().as_u16();
    let bytes = response.bytes().await.expect("a body").to_vec();
    (status, bytes)
}

/// Compare two error bodies that were **both produced by Go** — one directly, one through the
/// proxy.
///
/// `common::assert_error_bodies_match_except_known_gaps` cannot be used for a forwarded route: its
/// last assertion is that *our* `message` equals our `id`, which is true only while this server
/// writes untranslated errors ([D-092]). A forwarded body carries Go's translated message, so the
/// helper fails on a response that is in fact byte-identical apart from the request id.
fn assert_forwarded_bodies_match(go_body: &[u8], rs_body: &[u8], context: &str) {
    let go: serde_json::Value =
        serde_json::from_slice(go_body).unwrap_or_else(|e| panic!("{context}: Go's body: {e}"));
    let rs: serde_json::Value =
        serde_json::from_slice(rs_body).unwrap_or_else(|e| panic!("{context}: our body: {e}"));
    let strip = |value: &serde_json::Value| {
        let mut map = value.as_object().cloned().unwrap_or_default();
        map.remove("request_id");
        serde_json::Value::Object(map)
    };
    assert_eq!(
        strip(&go),
        strip(&rs),
        "{context}: a forwarded body must be Go's own, request id aside"
    );
}

/// The fields of a created user that two different accounts must still agree on.
///
/// `id`, `username`, `email` and the three timestamps cannot agree — the whole route exists to
/// allocate them. Everything else is a statement about the *handler*, and a divergence in any of
/// them is a port bug.
fn comparable(user: &serde_json::Value) -> serde_json::Value {
    let mut map = user.as_object().cloned().unwrap_or_default();
    for volatile in [
        "id",
        "username",
        "email",
        "create_at",
        "update_at",
        // `PreSave` stamps this from `GetMillis()` whenever a password is set, so it moves with
        // the clock exactly like the other two timestamps. It is not exempt on the read routes,
        // where both servers report the *same* stored row.
        "last_password_update",
    ] {
        map.remove(volatile);
    }
    serde_json::Value::Object(map)
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users — the anonymous branch
// ---------------------------------------------------------------------------------------------

/// An anonymous signup succeeds on both servers and the two bodies agree everywhere they can.
///
/// This is the `CreateUserFromSignup` branch: no `t`, no `iid`, no session at all.
#[tokio::test]
async fn an_anonymous_signup_agrees_field_for_field() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    scrub("anongo").await;
    scrub("anonrs").await;

    let body = |tag: &str| {
        serde_json::json!({
            "email": new_email(tag),
            "username": new_username(tag),
            "password": PLAIN_USER_PASSWORD,
        })
    };

    let (go_status, go_user) = post(&http, GO, "/api/v4/users", None, body("anongo")).await;
    let (rs_status, rs_user) = post(&http, RUST, "/api/v4/users", None, body("anonrs")).await;

    assert_eq!(go_status, 201, "Go refused the signup: {go_user}");
    assert_eq!(rs_status, 201, "we refused the signup: {rs_user}");
    assert_eq!(
        comparable(&go_user),
        comparable(&rs_user),
        "the two created users differ beyond their identities"
    );
    // The one field worth naming out loud: an anonymous signup is a plain member, whatever the
    // body asked for.
    assert_eq!(rs_user["roles"], serde_json::json!("system_user"));
    // `Sanitize(map[string]bool{})` strips these on the way out, so they must be absent or empty
    // on both. `password` is `json:"-"` and never appears at all.
    assert_eq!(rs_user.get("password"), None);
    assert_eq!(go_user.get("password"), None);

    // The three preferences `createUserOrGuest` writes after the insert. Nothing in the response
    // mentions them and no route this server serves reads them back, so the table is the only
    // oracle — and a port that skipped the write, or wrote a different value, would be invisible
    // to every other assertion in this file.
    //
    // `tutorial_step`'s **name is the user's own id**, so it is normalised to a placeholder
    // before the two sets are compared; the other two are constants.
    if let Some(pool) = pool().await {
        let read = async |user: &serde_json::Value| {
            let id = user["id"].as_str().expect("an id").to_owned();
            let mut rows: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT category, name, value FROM preferences WHERE userid = $1
                 ORDER BY category, name",
            )
            .bind(&id)
            .fetch_all(&pool)
            .await
            .expect("the preferences read");
            for row in &mut rows {
                if row.1 == id {
                    row.1 = "<the user's own id>".to_owned();
                }
            }
            rows
        };
        let go_prefs = read(&go_user).await;
        let rs_prefs = read(&rs_user).await;
        assert_eq!(go_prefs, rs_prefs, "the created users' preferences differ");
        assert_eq!(
            go_prefs.len(),
            3,
            "createUserOrGuest writes exactly three preferences: {go_prefs:?}"
        );
    }

    scrub("anongo").await;
    scrub("anonrs").await;
}

/// A create publishes exactly one `new_user`, carrying only the new user's id.
///
/// # The wait and the count are both scoped to this test's own account
///
/// An unscoped `collect_until(|f| f["event"] == "new_user")` is satisfied by any other suite's
/// create, and the count beside it then counts the stranger's frame. Both the predicate and the
/// tally below filter on `data.user_id` being the id this test just allocated, so a concurrent
/// create cannot make this pass or fail.
///
/// The event is deliberately *not* the user object: "this message goes to everyone", so it
/// carries an id and a client fetches the profile itself under its own permissions.
#[tokio::test]
async fn a_create_publishes_one_new_user_event() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    scrub("wsgo").await;
    scrub("wsrs").await;

    let mut go_probe = common::SocketProbe::connect(GO, &admin).await;
    let mut rs_probe = common::SocketProbe::connect(RUST, &admin).await;

    let mut ids = Vec::new();
    for (base, tag) in [(GO, "wsgo"), (RUST, "wsrs")] {
        let (status, user) = post(
            &http,
            base,
            "/api/v4/users",
            None,
            serde_json::json!({
                "email": new_email(tag),
                "username": new_username(tag),
                "password": PLAIN_USER_PASSWORD,
            }),
        )
        .await;
        assert_eq!(status, 201, "{tag}: {user}");
        ids.push(user["id"].as_str().expect("an id").to_owned());
    }

    let window = std::time::Duration::from_secs(5);
    let mine = |frames: &[serde_json::Value], id: &str| -> Vec<serde_json::Value> {
        frames
            .iter()
            .filter(|frame| {
                frame.get("event").and_then(|e| e.as_str()) == Some("new_user")
                    && frame
                        .get("data")
                        .and_then(|d| d.get("user_id"))
                        .and_then(|v| v.as_str())
                        == Some(id)
            })
            .cloned()
            .collect()
    };

    for (probe, id, who) in [
        (&mut go_probe, &ids[0], "Go"),
        (&mut rs_probe, &ids[1], "we"),
    ] {
        let id = id.clone();
        let arrived = probe
            .collect_until(window, move |frames| !mine(frames, &id).is_empty())
            .await;
        assert!(
            arrived,
            "{who} published no new_user for the created account"
        );
    }

    // Wait out a further window so a *second* frame for the same id would have arrived.
    for probe in [&mut go_probe, &mut rs_probe] {
        probe
            .collect_for(std::time::Duration::from_millis(600))
            .await;
    }

    for (probe, id, who) in [(&go_probe, &ids[0], "Go"), (&rs_probe, &ids[1], "we")] {
        let frames = mine(&probe.frames(), id);
        assert_eq!(
            frames.len(),
            1,
            "{who} published {} new_user frames for its own account",
            frames.len()
        );
        let data = frames[0]["data"].as_object().expect("a data object");
        assert_eq!(
            data.keys().collect::<Vec<_>>(),
            vec!["user_id"],
            "{who}: new_user must carry the id and nothing else"
        );
        assert_eq!(frames[0]["broadcast"]["user_id"], serde_json::json!(""));
        assert_eq!(frames[0]["broadcast"]["channel_id"], serde_json::json!(""));
        assert_eq!(frames[0]["broadcast"]["team_id"], serde_json::json!(""));
    }

    scrub("wsgo").await;
    scrub("wsrs").await;
}

/// `SanitizeInput(false)` clears what an anonymous caller is not allowed to assert.
///
/// Six of the fields it clears are visible in the response: `email_verified`, `auth_service`,
/// `auth_data`, `create_at`, `update_at` and `failed_attempts`. The body below asks for all of
/// them; both servers must ignore all of them.
///
/// `create_at` is the sharpest of the six, because it is the one a client could otherwise use to
/// backdate an account — and it is *also* cleared for a system admin, unlike the first three.
#[tokio::test]
async fn an_anonymous_signup_cannot_assert_its_own_verification_or_timestamps() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    scrub("sango").await;
    scrub("sanrs").await;

    let body = |tag: &str| {
        serde_json::json!({
            "email": new_email(tag),
            "username": new_username(tag),
            "password": PLAIN_USER_PASSWORD,
            "email_verified": true,
            "auth_service": "gitlab",
            "auth_data": "1234",
            "create_at": 1_000_000,
            "update_at": 2_000_000,
            "failed_attempts": 9,
            "roles": "system_admin",
            "mfa_active": true,
        })
    };

    let (go_status, go_user) = post(&http, GO, "/api/v4/users", None, body("sango")).await;
    let (rs_status, rs_user) = post(&http, RUST, "/api/v4/users", None, body("sanrs")).await;
    assert_eq!(go_status, 201, "Go refused: {go_user}");
    assert_eq!(rs_status, 201, "we refused: {rs_user}");
    assert_eq!(comparable(&go_user), comparable(&rs_user));

    assert_eq!(rs_user["roles"], serde_json::json!("system_user"));
    assert_ne!(rs_user["create_at"], serde_json::json!(1_000_000));
    assert_ne!(rs_user["update_at"], serde_json::json!(2_000_000));

    // And the flag in the table, which the response cannot show: `Sanitize` does not clear
    // `EmailVerified` but the field is `omitempty`, so `false` is simply absent from both bodies.
    // The row is the only oracle.
    if let Some(pool) = pool().await {
        for tag in ["sango", "sanrs"] {
            let verified: Option<bool> =
                sqlx::query_scalar("SELECT emailverified FROM users WHERE username = $1")
                    .bind(new_username(tag))
                    .fetch_one(&pool)
                    .await
                    .expect("the user exists");
            assert_eq!(
                verified,
                Some(false),
                "{tag} verified itself through the request body"
            );
        }
    }

    scrub("sango").await;
    scrub("sanrs").await;
}

/// An unsupported `locale` is replaced with `LocalizationSettings.DefaultClientLocale`, and a
/// supported one is kept.
///
/// `zz` passes `model.IsValidLocale` — it is a well-formed two-letter tag — so the only thing that
/// rejects it is the membership test against the twenty-three locales the server ships
/// translations for. A port that used `IsValidLocale` here would keep `zz` and this test is what
/// says so.
#[tokio::test]
async fn an_unsupported_locale_is_replaced_and_a_supported_one_is_not() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    for tag in ["locgo", "locrs", "keepgo", "keeprs"] {
        scrub(tag).await;
    }

    let body = |tag: &str, locale: &str| {
        serde_json::json!({
            "email": new_email(tag),
            "username": new_username(tag),
            "password": PLAIN_USER_PASSWORD,
            "locale": locale,
        })
    };

    let (_, go_reset) = post(&http, GO, "/api/v4/users", None, body("locgo", "zz")).await;
    let (_, rs_reset) = post(&http, RUST, "/api/v4/users", None, body("locrs", "zz")).await;
    assert_eq!(go_reset["locale"], rs_reset["locale"]);
    assert_eq!(rs_reset["locale"], serde_json::json!("en"));

    // `fr` is on the list; `en-au` is *not*, because the list is case-sensitive and ships `en-AU`.
    let (_, go_keep) = post(&http, GO, "/api/v4/users", None, body("keepgo", "fr")).await;
    let (_, rs_keep) = post(&http, RUST, "/api/v4/users", None, body("keeprs", "fr")).await;
    assert_eq!(go_keep["locale"], rs_keep["locale"]);
    assert_eq!(rs_keep["locale"], serde_json::json!("fr"));

    for tag in ["locgo", "locrs", "keepgo", "keeprs"] {
        scrub(tag).await;
    }
}

/// The refusals, compared as bodies. Each is a 400 on both servers with the same id.
///
/// One create per case is enough: the refusal happens before the insert, so nothing needs
/// scrubbing and the *same* body can go to both bases.
#[tokio::test]
async fn the_creation_refusals_agree_id_for_id() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    scrub("dupe").await;

    // A body that is not an object at all.
    let (go_status, go_body) = post_raw(&http, GO, "/api/v4/users", None, b"[]").await;
    let (rs_status, rs_body) = post_raw(&http, RUST, "/api/v4/users", None, b"[]").await;
    assert_eq!((go_status, rs_status), (400, 400));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a non-object body");

    // A password that fails every character rule the stack has switched on, plus the length rule.
    let short = serde_json::json!({
        "email": new_email("pwd"),
        "username": new_username("pwd"),
        "password": "a",
    });
    let (go_status, go_body) = post_raw(
        &http,
        GO,
        "/api/v4/users",
        None,
        &serde_json::to_vec(&short).expect("a body"),
    )
    .await;
    let (rs_status, rs_body) = post_raw(
        &http,
        RUST,
        "/api/v4/users",
        None,
        &serde_json::to_vec(&short).expect("a body"),
    )
    .await;
    assert_eq!(
        (go_status, rs_status),
        (400, 400),
        "a one-character password"
    );
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a short password");

    // A username that is not a legal username. `IsValid` refuses it inside the store, and the
    // AppError travels back out unchanged rather than becoming a create-specific id.
    let bad_name = serde_json::json!({
        "email": new_email("badname"),
        "username": "Not A Username",
        "password": PLAIN_USER_PASSWORD,
    });
    let (go_status, go_body) = post_raw(
        &http,
        GO,
        "/api/v4/users",
        None,
        &serde_json::to_vec(&bad_name).expect("a body"),
    )
    .await;
    let (rs_status, rs_body) = post_raw(
        &http,
        RUST,
        "/api/v4/users",
        None,
        &serde_json::to_vec(&bad_name).expect("a body"),
    )
    .await;
    assert_eq!((go_status, rs_status), (400, 400), "a malformed username");
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a malformed username");

    // Duplicates, and **the two constraints are tested separately on purpose**. A body that
    // repeats both the e-mail and the username violates both unique indexes at once, and
    // Postgres reports whichever index it happened to check — so the id would be
    // `email_exists` or `username_exists` depending on nothing the port controls, and a test
    // that sent both could not tell the two apart. One duplicated field each.
    let (created, _) = post(
        &http,
        GO,
        "/api/v4/users",
        None,
        serde_json::json!({
            "email": new_email("dupe"),
            "username": new_username("dupe"),
            "password": PLAIN_USER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(created, 201, "the fixture account could not be created");

    for (case, expected, body) in [
        (
            "a duplicate e-mail",
            "app.user.save.email_exists.app_error",
            serde_json::json!({
                "email": new_email("dupe"),
                "username": new_username("dupemail"),
                "password": PLAIN_USER_PASSWORD,
            }),
        ),
        (
            "a duplicate username",
            "app.user.save.username_exists.app_error",
            serde_json::json!({
                "email": new_email("dupename"),
                "username": new_username("dupe"),
                "password": PLAIN_USER_PASSWORD,
            }),
        ),
    ] {
        let raw = serde_json::to_vec(&body).expect("a body");
        let (go_status, go_body) = post_raw(&http, GO, "/api/v4/users", None, &raw).await;
        let (rs_status, rs_body) = post_raw(&http, RUST, "/api/v4/users", None, &raw).await;
        assert_eq!((go_status, rs_status), (400, 400), "{case}");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, case);
        assert_eq!(go["id"], serde_json::json!(expected), "{case}");
    }

    for tag in ["dupe", "dupemail", "dupename"] {
        scrub(tag).await;
    }
}

/// A request carrying `?t=` or `?iid=` is forwarded, and both spellings behave identically across
/// the pair.
///
/// The values are deliberately junk, so what is being compared is the *branch*: `t` reaches
/// `GetTokenById` and 404s, `iid` reaches `Team().GetByInviteId` and 404s. Neither writes
/// anything, which is the second assertion — a handler that took the signup branch for one of
/// these would have created an account instead.
#[tokio::test]
async fn the_token_and_invite_branches_forward_and_write_nothing() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    scrub("fwd").await;

    let body = serde_json::json!({
        "email": new_email("fwd"),
        "username": new_username("fwd"),
        "password": PLAIN_USER_PASSWORD,
    });
    let raw = serde_json::to_vec(&body).expect("a body");

    for query in [
        "?t=nosuchtokennosuchtokennosuchtoken",
        "?iid=nosuchinviteid",
    ] {
        let path = format!("/api/v4/users{query}");
        let (go_status, go_body) = post_raw(&http, GO, &path, None, &raw).await;
        let (rs_status, rs_body) = post_raw(&http, RUST, &path, None, &raw).await;
        assert_eq!(go_status, rs_status, "{query}: the statuses differ");
        assert_forwarded_bodies_match(&go_body, &rs_body, query);
        assert_ne!(go_status, 201, "{query} was supposed to be refused");
    }

    // Neither branch created the account, on either server.
    if let Some(pool) = pool().await {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE username = $1")
            .bind(new_username("fwd"))
            .fetch_one(&pool)
            .await
            .expect("a count");
        assert_eq!(count, 0, "a forwarded create left an account behind");
    }

    scrub("fwd").await;
}

/// `t` wins over `iid` when both are present.
///
/// Both forward here, so the observable difference is the *error*: a bad token is
/// `api.user.create_user.signup_link_invalid.app_error` and a bad invite id is
/// `app.team.get_by_invite_id.finding.app_error`. Sending both and getting the first is what
/// pins the precedence.
#[tokio::test]
async fn the_token_branch_wins_over_the_invite_branch() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let body = serde_json::json!({
        "email": new_email("prec"),
        "username": new_username("prec"),
        "password": PLAIN_USER_PASSWORD,
    });
    let raw = serde_json::to_vec(&body).expect("a body");
    let path = "/api/v4/users?t=nosuchtokennosuchtokennosuchtoken&iid=nosuchinviteid";

    let (go_status, go_body) = post_raw(&http, GO, path, None, &raw).await;
    let (rs_status, rs_body) = post_raw(&http, RUST, path, None, &raw).await;
    assert_eq!(go_status, rs_status);
    assert_forwarded_bodies_match(&go_body, &rs_body, "both query parameters");

    let go: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    assert_eq!(
        go["id"],
        serde_json::json!("api.user.create_user.signup_link_invalid.app_error"),
        "the invite-id branch was taken with a token present"
    );
}

/// A system admin's create takes a different branch and the difference is visible.
///
/// `SanitizeInput(true)` leaves `email_verified`, `auth_service` and `auth_data` alone, so an
/// admin *can* create a pre-verified account where an anonymous caller cannot — and
/// `CreateUserAsAdmin` skips `IsUserSignUpAllowed` entirely.
#[tokio::test]
async fn a_system_admin_may_create_a_pre_verified_account() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    scrub("admgo").await;
    scrub("admrs").await;

    let body = |tag: &str| {
        serde_json::json!({
            "email": new_email(tag),
            "username": new_username(tag),
            "password": PLAIN_USER_PASSWORD,
            "email_verified": true,
        })
    };

    let (go_status, go_user) = post(&http, GO, "/api/v4/users", Some(&admin), body("admgo")).await;
    let (rs_status, rs_user) =
        post(&http, RUST, "/api/v4/users", Some(&admin), body("admrs")).await;
    assert_eq!(go_status, 201, "Go refused the admin create: {go_user}");
    assert_eq!(rs_status, 201, "we refused the admin create: {rs_user}");
    assert_eq!(comparable(&go_user), comparable(&rs_user));

    // The flag is not in the body (`Sanitize` leaves it, but `omitempty` hides `false` and
    // `SanitizeProfile` is not called here) — the row is the oracle, and it must be **true** on
    // both, where the anonymous test above required false.
    if let Some(pool) = pool().await {
        for tag in ["admgo", "admrs"] {
            let verified: Option<bool> =
                sqlx::query_scalar("SELECT emailverified FROM users WHERE username = $1")
                    .bind(new_username(tag))
                    .fetch_one(&pool)
                    .await
                    .expect("the user exists");
            assert_eq!(
                verified,
                Some(true),
                "{tag}: the admin's email_verified was discarded"
            );
        }
    }

    scrub("admgo").await;
    scrub("admrs").await;
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users/email/verify/send
// ---------------------------------------------------------------------------------------------

/// The two served cases: a missing address is a 400, an unmatched one is `{"status":"OK"}`, and
/// **neither writes a token**.
#[tokio::test]
async fn the_verification_send_refusals_write_no_token() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let path = "/api/v4/users/email/verify/send";

    for body in [
        &b"{}"[..],
        br#"{"email":""}"#,
        // A non-string value: `MapFromJSON` returns the zero map on Go, serde fails the whole
        // object here, and both therefore see a missing `email`.
        br#"{"email":7}"#,
        b"not json at all",
    ] {
        let (go_status, go_body) = post_raw(&http, GO, path, None, body).await;
        let (rs_status, rs_body) = post_raw(&http, RUST, path, None, body).await;
        assert_eq!(
            (go_status, rs_status),
            (400, 400),
            "{}: expected a 400 from both",
            String::from_utf8_lossy(body)
        );
        assert_error_bodies_match_except_known_gaps(
            &go_body,
            &rs_body,
            &String::from_utf8_lossy(body),
        );
    }

    let nobody = "mmrsnobodyverify@mmrs.invalid";
    let before = tokens_naming(nobody).await;
    let unmatched = format!(r#"{{"email":"{nobody}"}}"#);
    let (go_status, go_body) = post_raw(&http, GO, path, None, unmatched.as_bytes()).await;
    let (rs_status, rs_body) = post_raw(&http, RUST, path, None, unmatched.as_bytes()).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(
        go_body, rs_body,
        "the unmatched-address body must be byte-identical"
    );
    assert_eq!(
        go_body, br#"{"status":"OK"}"#,
        "ReturnStatusOK writes no trailing newline"
    );
    assert_eq!(
        tokens_naming(nobody).await,
        before,
        "an unmatched address minted a token"
    );
}

/// A matched address forwards, and the forward mints **exactly one** token — Go's.
///
/// This is the assertion the split exists for. `SendEmailVerification` saves a `Tokens` row and
/// only then tries to send, so a port that served the lookup and then handed over would either
/// write a second row or leave one behind for a request it did not finish. One row after one
/// request through mm-api says neither happened.
#[tokio::test]
async fn a_matched_verification_send_forwards_and_mints_exactly_one_token() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    scrub("vsend").await;

    let created = post(
        &http,
        GO,
        "/api/v4/users",
        None,
        serde_json::json!({
            "email": new_email("vsend"),
            "username": new_username("vsend"),
            "password": PLAIN_USER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(created.0, 201, "the fixture account: {}", created.1);

    let before = tokens_naming(&new_email("vsend")).await;
    let body = format!(r#"{{"email":"{}"}}"#, new_email("vsend"));
    let (rs_status, rs_body) = post_raw(
        &http,
        RUST,
        "/api/v4/users/email/verify/send",
        None,
        body.as_bytes(),
    )
    .await;
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    assert_eq!(
        tokens_naming(&new_email("vsend")).await - before,
        1,
        "the forward should have minted exactly one token"
    );

    if let Some(pool) = pool().await {
        let _ = sqlx::query("DELETE FROM tokens WHERE extra LIKE $1")
            .bind(format!("%{}%", new_email("vsend")))
            .execute(&pool)
            .await;
    }
    scrub("vsend").await;
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users/password/reset/send
// ---------------------------------------------------------------------------------------------

/// The served refusals, and none of them writes a token.
///
/// The SSO case is set up by writing `AuthData` directly: there is no route that converts an
/// existing e-mail account to SSO on an unlicensed server, and the refusal it selects is the one
/// a reader is most likely to get the *order* of wrong (it is checked after `IsRemote`).
#[tokio::test]
async fn the_password_reset_send_refusals_write_no_token() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let path = "/api/v4/users/password/reset/send";
    scrub("prsso").await;

    for body in [&b"{}"[..], br#"{"email":""}"#, br#"{"email":7}"#] {
        let (go_status, go_body) = post_raw(&http, GO, path, None, body).await;
        let (rs_status, rs_body) = post_raw(&http, RUST, path, None, body).await;
        assert_eq!(
            (go_status, rs_status),
            (400, 400),
            "{}",
            String::from_utf8_lossy(body)
        );
        assert_error_bodies_match_except_known_gaps(
            &go_body,
            &rs_body,
            &String::from_utf8_lossy(body),
        );
    }

    let nobody = "mmrsnobodyreset@mmrs.invalid";
    let before = tokens_naming(nobody).await;
    let unmatched = format!(r#"{{"email":"{nobody}"}}"#);
    let (go_status, go_body) = post_raw(&http, GO, path, None, unmatched.as_bytes()).await;
    let (rs_status, rs_body) = post_raw(&http, RUST, path, None, unmatched.as_bytes()).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body);
    assert_eq!(go_body, br#"{"status":"OK"}"#);
    assert_eq!(
        tokens_naming(nobody).await,
        before,
        "an unmatched address minted a recovery token"
    );

    // An SSO account: `AuthData` non-empty is the second refusal.
    let created = post(
        &http,
        GO,
        "/api/v4/users",
        None,
        serde_json::json!({
            "email": new_email("prsso"),
            "username": new_username("prsso"),
            "password": PLAIN_USER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(created.0, 201, "the fixture account: {}", created.1);

    if let Some(pool) = pool().await {
        sqlx::query("UPDATE users SET authdata = $1, authservice = 'gitlab' WHERE username = $2")
            .bind("mmrs-sso-1")
            .bind(new_username("prsso"))
            .execute(&pool)
            .await
            .expect("the fixture row updates");
    }

    let before = tokens_naming(&new_email("prsso")).await;
    let sso = format!(r#"{{"email":"{}"}}"#, new_email("prsso"));
    let (go_status, go_body) = post_raw(&http, GO, path, None, sso.as_bytes()).await;
    let (rs_status, rs_body) = post_raw(&http, RUST, path, None, sso.as_bytes()).await;
    assert_eq!(
        (go_status, rs_status),
        (400, 400),
        "an SSO account: go={} rs={}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "an SSO account");
    let go: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    assert_eq!(
        go["id"],
        serde_json::json!("api.user.send_password_reset.sso.app_error")
    );
    assert_eq!(
        tokens_naming(&new_email("prsso")).await,
        before,
        "the SSO refusal minted a recovery token"
    );

    scrub("prsso").await;
}

/// An ordinary account forwards, and the forward mints exactly one recovery token.
///
/// The status is whatever the stack's (absent) mail server produces — a 500 here — and the point
/// is that both servers produce the *same* one, from the same row.
#[tokio::test]
async fn a_matched_password_reset_send_forwards_and_mints_exactly_one_token() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    scrub("prok").await;

    let created = post(
        &http,
        GO,
        "/api/v4/users",
        None,
        serde_json::json!({
            "email": new_email("prok"),
            "username": new_username("prok"),
            "password": PLAIN_USER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(created.0, 201, "the fixture account: {}", created.1);

    let before = tokens_naming(&new_email("prok")).await;
    let body = format!(r#"{{"email":"{}"}}"#, new_email("prok"));
    let (rs_status, _) = post_raw(
        &http,
        RUST,
        "/api/v4/users/password/reset/send",
        None,
        body.as_bytes(),
    )
    .await;
    let after_rs = tokens_naming(&new_email("prok")).await;
    assert_eq!(
        after_rs - before,
        1,
        "the forward should have minted exactly one recovery token (status {rs_status})"
    );

    // Go answers the same way for the same account. `CreatePasswordRecoveryToken` deletes this
    // user's existing recovery tokens first, so the count does not move a second time.
    let (go_status, _) = post_raw(
        &http,
        GO,
        "/api/v4/users/password/reset/send",
        None,
        body.as_bytes(),
    )
    .await;
    assert_eq!(go_status, rs_status, "the two servers answered differently");

    if let Some(pool) = pool().await {
        let _ = sqlx::query("DELETE FROM tokens WHERE extra LIKE $1")
            .bind(format!("%{}%", new_email("prok")))
            .execute(&pool)
            .await;
    }
    scrub("prok").await;
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/users/{user_id}/email/verify/member
// ---------------------------------------------------------------------------------------------

/// An admin verifies somebody's address, the two bodies agree, and the row moves.
#[tokio::test]
async fn verifying_a_member_without_a_token_agrees_and_sets_the_flag() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    scrub("vergo").await;
    scrub("verrs").await;

    let mut ids = Vec::new();
    for tag in ["vergo", "verrs"] {
        let (status, user) = post(
            &http,
            GO,
            "/api/v4/users",
            None,
            serde_json::json!({
                "email": new_email(tag),
                "username": new_username(tag),
                "password": PLAIN_USER_PASSWORD,
            }),
        )
        .await;
        assert_eq!(status, 201, "the fixture account {tag}: {user}");
        ids.push(user["id"].as_str().expect("an id").to_owned());
    }

    let (go_status, go_body) = post_raw(
        &http,
        GO,
        &format!("/api/v4/users/{}/email/verify/member", ids[0]),
        Some(&admin),
        b"",
    )
    .await;
    let (rs_status, rs_body) = post_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{}/email/verify/member", ids[1]),
        Some(&admin),
        b"",
    )
    .await;
    assert_eq!((go_status, rs_status), (200, 200));

    let go: serde_json::Value = serde_json::from_slice(&go_body).expect("JSON");
    let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("JSON");
    assert_eq!(
        comparable(&go),
        comparable(&rs),
        "the two sanitised bodies differ"
    );
    // `SanitizeProfile` → `ClearNonProfileFields` clears `EmailVerified` unconditionally, so the
    // response never reports the change it just made. `omitempty` then hides the `false`.
    assert_eq!(rs.get("email_verified"), None);

    if let Some(pool) = pool().await {
        for (tag, id) in ["vergo", "verrs"].iter().zip(&ids) {
            let verified: Option<bool> =
                sqlx::query_scalar("SELECT emailverified FROM users WHERE id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await
                    .expect("the user exists");
            assert_eq!(verified, Some(true), "{tag} was not verified");
        }
    }

    scrub("vergo").await;
    scrub("verrs").await;
}

/// The lookup runs **before** the permission check, so an unprivileged caller asking about an id
/// that names nobody gets a 404 and one asking about a real account gets a 403.
///
/// Two different statuses for two ids from the same caller is the whole assertion: swapping the
/// order would make both a 403, which is the "safer" shape and is not Go's.
#[tokio::test]
async fn the_lookup_precedes_the_permission_check() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;
    scrub("permu").await;
    scrub("permt").await;

    // The caller: an ordinary account with no permissions, logged in against Go.
    let (status, caller) = post(
        &http,
        GO,
        "/api/v4/users",
        None,
        serde_json::json!({
            "email": new_email("permu"),
            "username": new_username("permu"),
            "password": PLAIN_USER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(status, 201, "the caller account: {caller}");
    let (status, target) = post(
        &http,
        GO,
        "/api/v4/users",
        None,
        serde_json::json!({
            "email": new_email("permt"),
            "username": new_username("permt"),
            "password": PLAIN_USER_PASSWORD,
        }),
    )
    .await;
    assert_eq!(status, 201, "the target account: {target}");
    let target_id = target["id"].as_str().expect("an id").to_owned();

    let login = http
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({
            "login_id": new_username("permu"),
            "password": PLAIN_USER_PASSWORD,
        }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(login.status(), 200, "the caller cannot log in");
    let caller_token = login
        .headers()
        .get("token")
        .expect("a token header")
        .to_str()
        .expect("ASCII")
        .to_owned();

    // A well-formed id that names nobody: 404 on both, *before* the 403 that the same caller
    // gets for a real id.
    let nobody = "abcdefghijklmnopqrstuvwxyz";
    let (go_status, go_body) = post_raw(
        &http,
        GO,
        &format!("/api/v4/users/{nobody}/email/verify/member"),
        Some(&caller_token),
        b"",
    )
    .await;
    let (rs_status, rs_body) = post_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{nobody}/email/verify/member"),
        Some(&caller_token),
        b"",
    )
    .await;
    assert_eq!((go_status, rs_status), (404, 404), "a nonexistent target");
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a nonexistent target");

    let (go_status, go_body) = post_raw(
        &http,
        GO,
        &format!("/api/v4/users/{target_id}/email/verify/member"),
        Some(&caller_token),
        b"",
    )
    .await;
    let (rs_status, rs_body) = post_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{target_id}/email/verify/member"),
        Some(&caller_token),
        b"",
    )
    .await;
    assert_eq!((go_status, rs_status), (403, 403), "a real target");
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a real target");

    // A well-formed-looking but wrong-length id is a 400 from `RequireUserId`, before either.
    //
    // **It has to stay inside the mux charset.** Go registers `{user_id:[A-Za-z0-9]+}`, so
    // `not-an-id` never reaches the handler at all — gorilla answers **404** for the unmatched
    // route, and the first draft of this test asserted a 400 against that. Twenty-five
    // alphanumeric characters match the route and fail `IsValidId`, which is the branch meant
    // here.
    let short_id = "abcdefghijklmnopqrstuvwxy";
    let (go_status, go_body) = post_raw(
        &http,
        GO,
        &format!("/api/v4/users/{short_id}/email/verify/member"),
        Some(&admin),
        b"",
    )
    .await;
    let (rs_status, rs_body) = post_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{short_id}/email/verify/member"),
        Some(&admin),
        b"",
    )
    .await;
    assert_eq!((go_status, rs_status), (400, 400), "a malformed id");
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a malformed id");

    // And with no credentials at all: this route is `APISessionRequired`.
    let (go_status, _) = post_raw(
        &http,
        GO,
        &format!("/api/v4/users/{target_id}/email/verify/member"),
        None,
        b"",
    )
    .await;
    let (rs_status, _) = post_raw(
        &http,
        RUST,
        &format!("/api/v4/users/{target_id}/email/verify/member"),
        None,
        b"",
    )
    .await;
    assert_eq!((go_status, rs_status), (401, 401), "no credentials");

    scrub("permu").await;
    scrub("permt").await;
}
