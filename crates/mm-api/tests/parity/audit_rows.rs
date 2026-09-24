//! Cross-server parity for the `Audits` rows `c.LogAudit` and `c.LogAuditWithUserId` write
//! (web/context.go:95, :103) — [D-270].
//!
//! ```sh
//! scripts/parity.sh --test parity audit_rows
//! ```
//!
//! Every family whose handlers call `LogAudit` makes the same request against Go and against this
//! server, and the rows each wrote are compared: `action` (the path), `extrainfo`, `userid`,
//! `sessionid` and `ipaddress`, with the ids that differ between the two runs replaced by what they
//! name. The requests are made by sessions minted for this suite alone, so a row is found by its
//! session (or, for the rows written before a session exists, by a text only this suite uses)
//! rather than by time alone: the shared administrator's rows belong to every other suite too.
//!
//! Refusals are covered where Go writes a row on them — the `"attempt"` and `"fail - …"` rows of
//! the command, webhook, licence and token families — because a row written on a refusal is the
//! half of this a success-only test cannot see.

use crate::common;

use common::{
    GO, RUST, client, create_channel_typed, create_plain_user, create_team, fixture_pool,
    go_minted_token, stack_enabled,
};

/// One `Audits` row, as compared: action, extra info, user, session, address.
type Row = (String, String, String, String, String);

/// A request on one server: method, path, bearer token, JSON body.
struct Call<'a> {
    method: &'a str,
    path: String,
    token: &'a str,
    body: Option<serde_json::Value>,
}

impl<'a> Call<'a> {
    fn new(method: &'a str, path: impl Into<String>, token: &'a str) -> Self {
        Self {
            method,
            path: path.into(),
            token,
            body: None,
        }
    }

    fn body(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }
}

/// `(status, body)` of one call, asserting a `RUST` answer came from this server.
async fn send(http: &reqwest::Client, base: &str, call: &Call<'_>) -> (u16, serde_json::Value) {
    let method = reqwest::Method::from_bytes(call.method.as_bytes()).expect("a method");
    let mut request = http
        .request(method, format!("{base}{}", call.path))
        .header("Authorization", format!("Bearer {}", call.token));
    if let Some(body) = &call.body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust"),
            "{} {} was forwarded, so its rows are Go's",
            call.method,
            call.path
        );
    }
    let text = response.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or_default())
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// The rows written on `action` since `since` by one of `sessions`, for one of `users`, or
/// carrying one of `markers` in their text.
async fn rows(
    pool: &sqlx::PgPool,
    action: &str,
    since: i64,
    sessions: &[&str],
    users: &[&str],
    markers: &[&str],
) -> Vec<Row> {
    let sessions: Vec<String> = sessions.iter().map(|s| (*s).to_owned()).collect();
    let users: Vec<String> = users.iter().map(|s| (*s).to_owned()).collect();
    let markers: Vec<String> = markers.iter().map(|m| format!("%{m}%")).collect();
    let mut found: Vec<Row> = sqlx::query_as(
        "SELECT action, extrainfo, userid, sessionid, ipaddress FROM audits
          WHERE action = $1 AND createat >= $2
            AND (sessionid = ANY($3) OR userid = ANY($4) OR extrainfo LIKE ANY($5))",
    )
    .bind(action)
    .bind(since)
    .bind(&sessions)
    .bind(&users)
    .bind(&markers)
    .fetch_all(pool)
    .await
    .expect("the audit rows");
    found.sort();
    found
}

/// `row` with every `(from, to)` substituted, and any other 26-character id left as `<id>`.
fn named(row: &Row, subs: &[(String, String)]) -> Row {
    let name = |text: &str| -> String {
        let mut out = subs
            .iter()
            .fold(text.to_owned(), |acc, (from, to)| acc.replace(from, to));
        out = mask_ids(&out);
        out
    };
    (
        name(&row.0),
        name(&row.1),
        name(&row.2),
        name(&row.3),
        row.4.clone(),
    )
}

fn mask_ids(text: &str) -> String {
    let mut out = String::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.len() == 26 {
            out.push_str("<id>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

async fn session_id_of(pool: &sqlx::PgPool, token: &str) -> String {
    sqlx::query_scalar("SELECT id FROM sessions WHERE token = $1")
        .bind(token)
        .fetch_one(pool)
        .await
        .expect("the session row")
}

/// The suite's two callers: a system administrator and a plain member of `team`, each with a
/// session of their own.
struct Callers {
    team: String,
    admin_id: String,
    admin: String,
    admin_session: String,
    plain_id: String,
    plain: String,
    plain_session: String,
}

async fn callers(http: &reqwest::Client, pool: &sqlx::PgPool, tag: &str) -> Callers {
    let shared_admin = go_minted_token(http).await;
    let team = create_team(http, &shared_admin, &format!("{tag}t")).await;
    let admin_user = create_plain_user(http, &shared_admin, &team, &format!("{tag}a")).await;
    assert!(common::set_user_roles(&admin_user.id, "system_user system_admin").await);
    // A session minted after the role change carries it.
    let admin = common::login_plain_user(http, &format!("{tag}a")).await;
    let plain = create_plain_user(http, &shared_admin, &team, &format!("{tag}p")).await;
    Callers {
        admin_session: session_id_of(pool, &admin).await,
        plain_session: session_id_of(pool, &plain.token).await,
        team,
        admin_id: admin_user.id,
        admin,
        plain_id: plain.id,
        plain: plain.token,
    }
}

impl Callers {
    fn subs(&self) -> Vec<(String, String)> {
        vec![
            (self.admin_session.clone(), "<admin-session>".to_owned()),
            (self.plain_session.clone(), "<plain-session>".to_owned()),
            (self.admin_id.clone(), "<admin>".to_owned()),
            (self.plain_id.clone(), "<plain>".to_owned()),
            (self.team.clone(), "<team>".to_owned()),
        ]
    }

    fn sessions(&self) -> [&str; 2] {
        [&self.admin_session, &self.plain_session]
    }
}

/// Run `calls` on one server and return the rows its sessions wrote on their paths, named.
async fn run(
    http: &reqwest::Client,
    pool: &sqlx::PgPool,
    base: &str,
    who: &Callers,
    calls: &[Call<'_>],
    subs: &[(String, String)],
) -> Vec<(u16, Vec<Row>)> {
    let mut out = Vec::new();
    for call in calls {
        let since = now_millis() - 1;
        let (status, _) = send(http, base, call).await;
        let found = rows(pool, &call.path, since, &who.sessions(), &[], &[]).await;
        let mut all_subs = who.subs();
        all_subs.extend(subs.iter().cloned());
        out.push((
            status,
            found.iter().map(|row| named(row, &all_subs)).collect(),
        ));
    }
    out
}

fn extras(outcome: &[(u16, Vec<Row>)]) -> Vec<Vec<String>> {
    outcome
        .iter()
        .map(|(_, rows)| rows.iter().map(|r| r.1.clone()).collect())
        .collect()
}

/// The channel family: create, patch, privacy, add member, archive, restore — each a success-only
/// row, and `updateChannel`'s carrying the **submitted** name.
#[tokio::test]
async fn the_channel_writes_leave_the_rows_go_leaves() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let who = callers(&http, &pool, "audch").await;

    let mut results = Vec::new();
    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let name = format!("mmrs-parity-audch{side}");
        let since = now_millis() - 1;
        let (status, created) = send(
            &http,
            base,
            &Call::new("POST", "/api/v4/channels", &who.admin).body(serde_json::json!({
                "team_id": who.team, "name": name, "display_name": "audit", "type": "O"
            })),
        )
        .await;
        assert_eq!(status, 201, "{base} created the channel");
        let channel = created["id"].as_str().expect("an id").to_owned();
        let create_rows = rows(&pool, "/api/v4/channels", since, &who.sessions(), &[], &[]).await;
        let subs = vec![
            (channel.clone(), "<channel>".to_owned()),
            (name.clone(), "<name>".to_owned()),
        ];
        let calls = [
            Call::new(
                "PUT",
                format!("/api/v4/channels/{channel}/patch"),
                &who.admin,
            )
            .body(serde_json::json!({ "header": "audited" })),
            // The body's `name` is omitted, so Go's row is `name=` — the submitted name, not the
            // channel's.
            Call::new("PUT", format!("/api/v4/channels/{channel}"), &who.admin)
                .body(serde_json::json!({ "id": channel, "display_name": "audit two" })),
            Call::new(
                "PUT",
                format!("/api/v4/channels/{channel}/privacy"),
                &who.admin,
            )
            .body(serde_json::json!({ "privacy": "P" })),
            Call::new(
                "POST",
                format!("/api/v4/channels/{channel}/members"),
                &who.admin,
            )
            .body(serde_json::json!({ "user_id": who.plain_id })),
            // A second add of the same member: already there, so no row.
            Call::new(
                "POST",
                format!("/api/v4/channels/{channel}/members"),
                &who.admin,
            )
            .body(serde_json::json!({ "user_id": who.plain_id })),
            Call::new("DELETE", format!("/api/v4/channels/{channel}"), &who.admin),
            Call::new(
                "POST",
                format!("/api/v4/channels/{channel}/restore"),
                &who.admin,
            ),
            // A plain **member** of a private channel may archive it (`delete_private_channel` is a
            // channel-user permission), so this is a second success, under the member's session.
            Call::new("DELETE", format!("/api/v4/channels/{channel}"), &who.plain),
            // And a refusal writes nothing: the member's restore needs `manage_team`.
            Call::new(
                "POST",
                format!("/api/v4/channels/{channel}/restore"),
                &who.plain,
            ),
        ];
        let mut outcome = run(&http, &pool, base, &who, &calls, &subs).await;
        let mut all_subs = who.subs();
        all_subs.extend(subs.iter().cloned());
        outcome.insert(
            0,
            (
                status,
                create_rows.iter().map(|r| named(r, &all_subs)).collect(),
            ),
        );
        results.push(outcome);
    }
    let rust = results.pop().expect("two runs");
    let go = results.pop().expect("two runs");
    assert_eq!(
        extras(&go),
        [
            vec!["name=<name>"],
            vec![""],
            vec!["name="],
            vec!["name=<name>"],
            vec!["name=<name> user_id=<plain>"],
            vec![],
            vec!["name=<name>"],
            vec!["name=<name>"],
            vec!["name=<name>"],
            vec![],
        ],
        "what Go wrote: {go:#?}"
    );
    assert_eq!(rust, go);
}

/// The command family: `"attempt"` before the checks, `"fail - …"` on the refusals Go names, and
/// `"success"` — including a delete of an unknown command, whose `"attempt"` precedes the fetch.
#[tokio::test]
async fn the_command_writes_leave_attempt_fail_and_success_rows() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let who = callers(&http, &pool, "audcm").await;
    let unknown = "mmrsnocommand0000000000000";

    let mut results = Vec::new();
    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let since = now_millis() - 1;
        let (status, created) = send(
            &http,
            base,
            &Call::new("POST", "/api/v4/commands", &who.admin).body(serde_json::json!({
                "team_id": who.team, "trigger": format!("audcm{side}"),
                "url": "http://127.0.0.1:1/audit", "method": "P"
            })),
        )
        .await;
        assert_eq!(status, 201, "{base} created the command");
        let command = created["id"].as_str().expect("an id").to_owned();
        let create_rows = rows(&pool, "/api/v4/commands", since, &who.sessions(), &[], &[]).await;
        // The plain member refused on create: `"attempt"` only (the first rung writes no fail).
        let since = now_millis() - 1;
        let (refused, _) = send(
            &http,
            base,
            &Call::new("POST", "/api/v4/commands", &who.plain).body(serde_json::json!({
                "team_id": who.team, "trigger": format!("audcmx{side}"),
                "url": "http://127.0.0.1:1/audit", "method": "P"
            })),
        )
        .await;
        let refused_rows = rows(
            &pool,
            "/api/v4/commands",
            since,
            &[&who.plain_session],
            &[],
            &[],
        )
        .await;
        let subs = vec![(command.clone(), "<command>".to_owned())];
        let calls = [
            Call::new("PUT", format!("/api/v4/commands/{command}"), &who.admin).body(
                serde_json::json!({
                    "id": command, "team_id": who.team, "trigger": format!("audcm{side}"),
                    "url": "http://127.0.0.1:1/audit2", "method": "P"
                }),
            ),
            Call::new(
                "PUT",
                format!("/api/v4/commands/{command}/regen_token"),
                &who.admin,
            ),
            // The ownership ladder's first rung for a plain member: 404 plus a fail row.
            Call::new("DELETE", format!("/api/v4/commands/{command}"), &who.plain),
            Call::new("DELETE", format!("/api/v4/commands/{unknown}"), &who.admin),
            Call::new("DELETE", format!("/api/v4/commands/{command}"), &who.admin),
        ];
        let mut outcome = run(&http, &pool, base, &who, &calls, &subs).await;
        let all_subs: Vec<_> = who.subs().into_iter().chain(subs).collect();
        outcome.insert(
            0,
            (
                refused,
                refused_rows.iter().map(|r| named(r, &all_subs)).collect(),
            ),
        );
        outcome.insert(
            0,
            (
                status,
                create_rows.iter().map(|r| named(r, &all_subs)).collect(),
            ),
        );
        results.push(outcome);
    }
    let rust = results.pop().expect("two runs");
    let go = results.pop().expect("two runs");
    assert_eq!(
        extras(&go),
        [
            vec!["attempt", "success"],
            vec!["attempt"],
            vec!["attempt", "success"],
            vec!["attempt", "success"],
            vec!["attempt", "fail - inappropriate permissions"],
            vec!["attempt"],
            vec!["attempt", "success"],
        ],
        "what Go wrote: {go:#?}"
    );
    assert_eq!(rust, go);
}

/// The webhook family, incoming and outgoing: the single-hook reads audit too, and the incoming
/// delete writes **no** row on success — only its two refusals are audited.
#[tokio::test]
async fn the_webhook_routes_leave_the_rows_go_leaves() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let who = callers(&http, &pool, "audwh").await;

    let mut results = Vec::new();
    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let channel =
            create_channel_typed(&http, &who.admin, &who.team, &format!("audwh{side}"), "P").await;
        let since = now_millis() - 1;
        let (status, incoming) = send(
            &http,
            base,
            &Call::new("POST", "/api/v4/hooks/incoming", &who.admin)
                .body(serde_json::json!({ "channel_id": channel, "display_name": "audit" })),
        )
        .await;
        assert_eq!(status, 201, "{base} created the incoming hook");
        let incoming = incoming["id"].as_str().expect("an id").to_owned();
        let create_in = rows(
            &pool,
            "/api/v4/hooks/incoming",
            since,
            &who.sessions(),
            &[],
            &[],
        )
        .await;
        let since = now_millis() - 1;
        let (status, outgoing) = send(
            &http,
            base,
            &Call::new("POST", "/api/v4/hooks/outgoing", &who.admin).body(serde_json::json!({
                "team_id": who.team, "display_name": "audit",
                "trigger_words": [format!("audwh{side}")],
                "callback_urls": ["http://127.0.0.1:1/audit"]
            })),
        )
        .await;
        assert_eq!(status, 201, "{base} created the outgoing hook");
        let outgoing = outgoing["id"].as_str().expect("an id").to_owned();
        let create_out = rows(
            &pool,
            "/api/v4/hooks/outgoing",
            since,
            &who.sessions(),
            &[],
            &[],
        )
        .await;
        let subs = vec![
            (incoming.clone(), "<incoming>".to_owned()),
            (outgoing.clone(), "<outgoing>".to_owned()),
            (channel.clone(), "<channel>".to_owned()),
        ];
        let calls = [
            Call::new(
                "GET",
                format!("/api/v4/hooks/incoming/{incoming}"),
                &who.admin,
            ),
            // The plain member cannot read the private channel: attempt, then the bad-permissions
            // row.
            Call::new(
                "GET",
                format!("/api/v4/hooks/incoming/{incoming}"),
                &who.plain,
            ),
            Call::new(
                "PUT",
                format!("/api/v4/hooks/incoming/{incoming}"),
                &who.admin,
            )
            .body(
                serde_json::json!({ "id": incoming, "channel_id": channel, "display_name": "two" }),
            ),
            Call::new(
                "DELETE",
                format!("/api/v4/hooks/incoming/{incoming}"),
                &who.plain,
            ),
            Call::new(
                "DELETE",
                format!("/api/v4/hooks/incoming/{incoming}"),
                &who.admin,
            ),
            Call::new(
                "GET",
                format!("/api/v4/hooks/outgoing/{outgoing}"),
                &who.admin,
            ),
            Call::new(
                "POST",
                format!("/api/v4/hooks/outgoing/{outgoing}/regen_token"),
                &who.admin,
            ),
            Call::new(
                "DELETE",
                format!("/api/v4/hooks/outgoing/{outgoing}"),
                &who.plain,
            ),
            Call::new(
                "DELETE",
                format!("/api/v4/hooks/outgoing/{outgoing}"),
                &who.admin,
            ),
        ];
        let mut outcome = run(&http, &pool, base, &who, &calls, &subs).await;
        let all_subs: Vec<_> = who.subs().into_iter().chain(subs).collect();
        for (status, found) in [(201, create_out), (201, create_in)] {
            outcome.insert(
                0,
                (status, found.iter().map(|r| named(r, &all_subs)).collect()),
            );
        }
        results.push(outcome);
    }
    let rust = results.pop().expect("two runs");
    let go = results.pop().expect("two runs");
    assert_eq!(
        extras(&go),
        [
            vec!["attempt", "success"],
            vec!["attempt", "success"],
            vec!["attempt", "success"],
            vec!["attempt", "fail - bad permissions"],
            vec!["attempt", "success"],
            vec!["fail - bad permissions"],
            vec![],
            vec!["attempt", "success"],
            vec!["attempt", "success"],
            vec!["attempt"],
            vec!["attempt", "success"],
        ],
        "what Go wrote: {go:#?}"
    );
    assert_eq!(rust, go);
}

/// OAuth apps (a `client_id=` row on create, `"attempt"`/`"success"` around update and delete,
/// `"success"` alone on the secret), and the admin families whose Go handlers audit their first
/// statement: outgoing OAuth connections, the marketplace visit flag, product notices, the
/// onboarding read, the licence writes; plus role and team patches and the invite-id reset.
#[tokio::test]
async fn the_admin_families_leave_the_rows_go_leaves() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let who = callers(&http, &pool, "audad").await;
    // The licence routes read the active licence; no suite may plant one meanwhile.
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let role_name = common::plant_role("audad", "create_post")
        .await
        .expect("the planted role");
    let role_id = format!("mmrsrole{:0>18}", "audad");
    let _ = role_name;

    let mut results = Vec::new();
    for base in [GO, RUST] {
        let since = now_millis() - 1;
        let (status, app) = send(
            &http,
            base,
            &Call::new("POST", "/api/v4/oauth/apps", &who.admin).body(serde_json::json!({
                "name": "audit", "description": "audit", "homepage": "https://example.com",
                "callback_urls": ["https://example.com/cb"]
            })),
        )
        .await;
        assert_eq!(status, 201, "{base} created the app");
        let app_id = app["id"].as_str().expect("an id").to_owned();
        let create_rows = rows(
            &pool,
            "/api/v4/oauth/apps",
            since,
            &who.sessions(),
            &[],
            &[],
        )
        .await;
        let subs = vec![(app_id.clone(), "<app>".to_owned())];
        let mut app_body = app.clone();
        app_body["description"] = serde_json::json!("audit two");
        let calls = [
            Call::new("PUT", format!("/api/v4/oauth/apps/{app_id}"), &who.admin).body(app_body),
            Call::new(
                "POST",
                format!("/api/v4/oauth/apps/{app_id}/regen_secret"),
                &who.admin,
            ),
            Call::new("DELETE", format!("/api/v4/oauth/apps/{app_id}"), &who.plain),
            Call::new("DELETE", format!("/api/v4/oauth/apps/{app_id}"), &who.admin),
            Call::new("POST", "/api/v4/oauth/outgoing_connections", &who.plain)
                .body(serde_json::json!({})),
            Call::new(
                "GET",
                "/api/v4/plugins/marketplace/first_admin_visit",
                &who.plain,
            ),
            Call::new("PUT", "/api/v4/system/notices/view", &who.plain)
                .body(serde_json::json!(["audit-notice"])),
            Call::new("GET", "/api/v4/system/onboarding/complete", &who.plain),
            Call::new("POST", "/api/v4/trial-license", &who.plain).body(serde_json::json!({})),
            Call::new("DELETE", "/api/v4/license", &who.plain),
            // An unlicensed server's removal is served: `"attempt"` then `"success"`.
            Call::new("DELETE", "/api/v4/license", &who.admin),
            // No file: `"attempt"` is `addLicense`'s first statement.
            Call::new("POST", "/api/v4/license", &who.admin),
            Call::new("PUT", format!("/api/v4/roles/{role_id}/patch"), &who.admin)
                .body(serde_json::json!({ "permissions": ["create_post", "edit_post"] })),
            Call::new(
                "PUT",
                format!("/api/v4/teams/{}/patch", who.team),
                &who.admin,
            )
            .body(serde_json::json!({ "description": "audited" })),
            Call::new(
                "POST",
                format!("/api/v4/teams/{}/regenerate_invite_id", who.team),
                &who.admin,
            ),
        ];
        let mut outcome = run(&http, &pool, base, &who, &calls, &subs).await;
        let all_subs: Vec<_> = who.subs().into_iter().chain(subs).collect();
        outcome.insert(
            0,
            (
                status,
                create_rows.iter().map(|r| named(r, &all_subs)).collect(),
            ),
        );
        results.push(outcome);
    }
    let rust = results.pop().expect("two runs");
    let go = results.pop().expect("two runs");
    assert_eq!(
        extras(&go),
        [
            vec!["client_id=<app>"],
            vec!["attempt", "success"],
            vec!["success"],
            vec!["attempt"],
            vec!["attempt", "success"],
            vec!["attempt"],
            vec!["attempt"],
            vec!["attempt"],
            vec!["attempt"],
            vec!["attempt"],
            vec!["attempt"],
            vec!["attempt", "success"],
            vec!["attempt"],
            vec![""],
            vec![""],
            vec![""],
        ],
        "what Go wrote: {go:#?}"
    );
    assert_eq!(rust, go);
}

/// The user family: patch, roles, active off and on, auth, password (the `"failed"` and the
/// `"completed"` rows), the four token routes, a session revoke, terms of service, and the
/// member e-mail verification.
#[tokio::test]
async fn the_user_writes_leave_the_rows_go_leaves() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let who = callers(&http, &pool, "audus").await;
    let shared_admin = go_minted_token(&http).await;

    let mut results = Vec::new();
    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let subject =
            create_plain_user(&http, &shared_admin, &who.team, &format!("audus{side}")).await;
        // `EnableUserAccessTokens` is off on the stack and bots are exempt, so the token routes
        // run on a planted bot's token; a human's create is the 501, whose rows are compared too.
        let bot = common::plant_bot(&format!("audtk{side}"), &who.admin_id, 0)
            .await
            .expect("the bot");
        let (status, token) = send(
            &http,
            base,
            &Call::new("POST", format!("/api/v4/users/{bot}/tokens"), &who.admin)
                .body(serde_json::json!({ "description": "audit" })),
        )
        .await;
        assert_eq!(status, 200, "{base} created the token");
        let token_id = token["id"].as_str().expect("an id").to_owned();
        let subject_session = session_id_of(&pool, &subject.token).await;
        let subs = vec![
            (subject.id.clone(), "<subject>".to_owned()),
            (token_id.clone(), "<token>".to_owned()),
            (subject_session.clone(), "<subject-session>".to_owned()),
            (bot.clone(), "<bot>".to_owned()),
        ];
        let calls = [
            // First, while the subject still has the session: deactivation revokes it.
            Call::new(
                "POST",
                format!("/api/v4/users/{}/sessions/revoke", subject.id),
                &who.admin,
            )
            .body(serde_json::json!({ "session_id": subject_session })),
            Call::new(
                "POST",
                format!("/api/v4/users/{}/sessions/revoke/all", subject.id),
                &who.admin,
            ),
            Call::new(
                "PUT",
                format!("/api/v4/users/{}/patch", subject.id),
                &who.admin,
            )
            .body(serde_json::json!({ "nickname": "audited" })),
            Call::new(
                "PUT",
                format!("/api/v4/users/{}/roles", subject.id),
                &who.admin,
            )
            .body(serde_json::json!({ "roles": "system_user" })),
            Call::new(
                "PUT",
                format!("/api/v4/users/{}/active", subject.id),
                &who.admin,
            )
            .body(serde_json::json!({ "active": false })),
            Call::new(
                "PUT",
                format!("/api/v4/users/{}/active", subject.id),
                &who.admin,
            )
            .body(serde_json::json!({ "active": true })),
            // A plain member changing someone else's password: `"attempted"` then `"failed"`.
            Call::new(
                "PUT",
                format!("/api/v4/users/{}/password", subject.id),
                &who.plain,
            )
            .body(serde_json::json!({ "new_password": "Mmrs-Audit-12345" })),
            Call::new(
                "PUT",
                format!("/api/v4/users/{}/password", subject.id),
                &who.admin,
            )
            .body(serde_json::json!({ "new_password": "Mmrs-Audit-12345" })),
            Call::new(
                "POST",
                format!("/api/v4/users/{}/tokens", subject.id),
                &who.admin,
            )
            .body(serde_json::json!({ "description": "audit" })),
            Call::new("POST", "/api/v4/users/tokens/disable", &who.admin)
                .body(serde_json::json!({ "token_id": token_id })),
            Call::new("POST", "/api/v4/users/tokens/enable", &who.admin)
                .body(serde_json::json!({ "token_id": token_id })),
            Call::new("POST", "/api/v4/users/tokens/revoke", &who.admin)
                .body(serde_json::json!({ "token_id": token_id })),
            Call::new(
                "POST",
                format!("/api/v4/users/{}/email/verify/member", subject.id),
                &who.admin,
            ),
            // Last: a switch to an SSO service, which the password routes above could not follow.
            Call::new(
                "PUT",
                format!("/api/v4/users/{}/auth", subject.id),
                &who.admin,
            )
            .body(serde_json::json!({ "auth_service": "gitlab", "auth_data": format!("audit-{side}") })),
        ];
        let outcome = run(&http, &pool, base, &who, &calls, &subs).await;
        results.push(outcome);
        common::delete_plain_user(&http, &shared_admin, &subject.id).await;
        common::unplant_bot(&bot).await;
    }
    let rust = results.pop().expect("two runs");
    let go = results.pop().expect("two runs");
    assert_eq!(
        extras(&go),
        [
            vec![""],
            vec![""],
            vec![""],
            vec!["user=<subject> roles=system_user"],
            vec!["user_id=<subject> active=false"],
            vec!["user_id=<subject> active=true"],
            vec!["attempted", "failed"],
            vec!["attempted", "completed"],
            // A human's create is refused for `EnableUserAccessTokens` **after** Go's first
            // `LogAudit("")`.
            vec![""],
            vec!["", "success - token_id=<token>"],
            vec!["", "success - token_id=<token>"],
            vec!["", "success - token_id=<token>"],
            vec!["user verified"],
            vec!["updated user <subject> auth to service=gitlab"],
        ],
        "what Go wrote: {go:#?}"
    );
    assert_eq!(rust, go);
}

/// The login family: the four `LogAuditWithUserId` rows of a password login — the first two under
/// the caller's (here absent) session and the last under the **new** one, with its user's
/// `session_user=` suffix — and the two of a failed one; then the logout's single row.
#[tokio::test]
async fn a_login_and_a_logout_leave_the_rows_go_leaves() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let shared_admin = go_minted_token(&http).await;
    let team = create_team(&http, &shared_admin, "audlg").await;

    let mut results = Vec::new();
    for (base, side) in [(GO, "go"), (RUST, "rs")] {
        let tag = format!("audlg{side}");
        let user = create_plain_user(&http, &shared_admin, &team, &tag).await;
        let username = common::plain_username(&tag);
        let mut outcome = Vec::new();
        for password in ["wrong-password", common::PLAIN_USER_PASSWORD] {
            let since = now_millis() - 1;
            let response = http
                .post(format!("{base}/api/v4/users/login"))
                .json(&serde_json::json!({ "login_id": username, "password": password }))
                .send()
                .await
                .expect("the server answers");
            let status = response.status().as_u16();
            let token = response
                .headers()
                .get("token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let found = rows(
                &pool,
                "/api/v4/users/login",
                since,
                &[],
                &[&user.id],
                &[&format!("login_id={username}")],
            )
            .await;
            let mut subs = vec![
                (user.id.clone(), "<user>".to_owned()),
                (username.clone(), "<username>".to_owned()),
            ];
            if !token.is_empty() {
                subs.push((
                    session_id_of(&pool, &token).await,
                    "<new-session>".to_owned(),
                ));
                let logout_since = now_millis() - 1;
                let (logout, _) = send(
                    &http,
                    base,
                    &Call::new("POST", "/api/v4/users/logout", &token),
                )
                .await;
                let logout_rows = rows(
                    &pool,
                    "/api/v4/users/logout",
                    logout_since,
                    &[],
                    &[&user.id],
                    &[],
                )
                .await;
                outcome.push((status, found.iter().map(|r| named(r, &subs)).collect()));
                outcome.push((
                    logout,
                    logout_rows.iter().map(|r| named(r, &subs)).collect(),
                ));
                continue;
            }
            outcome.push((status, found.iter().map(|r| named(r, &subs)).collect()));
        }
        results.push(outcome);
        common::delete_plain_user(&http, &shared_admin, &user.id).await;
    }
    let rust: Vec<(u16, Vec<Row>)> = results.pop().expect("two runs");
    let go: Vec<(u16, Vec<Row>)> = results.pop().expect("two runs");
    assert_eq!(
        extras(&go),
        [
            vec![
                "attempt - login_id=<username>",
                "failure - login_id=<username>"
            ],
            vec![
                "attempt - login_id=<username>",
                "authenticated",
                "success session_user=<user>"
            ],
            vec![""],
        ],
        "what Go wrote: {go:#?}"
    );
    assert_eq!(rust, go);
}

/// `GET /users/{id}/audits` reads the rows both servers wrote, and answers them alike through
/// either — the read route is served here, and the table it reads is now written by both.
#[tokio::test]
async fn the_audit_history_reads_the_same_through_both_servers() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let who = callers(&http, &pool, "audrd").await;
    for base in [GO, RUST] {
        let (status, _) = send(
            &http,
            base,
            &Call::new(
                "PUT",
                format!("/api/v4/users/{}/patch", who.plain_id),
                &who.admin,
            )
            .body(serde_json::json!({ "nickname": format!("audrd{}", base.len()) })),
        )
        .await;
        assert_eq!(status, 200);
    }
    let path = format!("/api/v4/users/{}/audits", who.admin_id);
    let (go_status, go) = send(&http, GO, &Call::new("GET", &path, &who.admin)).await;
    let (rs_status, rs) = send(&http, RUST, &Call::new("GET", &path, &who.admin)).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(rs, go, "the two histories differ");
    let patches = go
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|row| {
                    row["action"]
                        .as_str()
                        .is_some_and(|a| a.ends_with("/patch"))
                })
                .count()
        })
        .unwrap_or_default();
    assert_eq!(patches, 2, "one row from each server: {go}");
}

/// Local-mode rows carry no user, no session and no address. `localCreateChannel`, a
/// `localPatchChannel` and a local archive, over each socket.
#[tokio::test]
async fn the_local_channel_writes_leave_anonymous_rows() {
    if !common::local_socket::sockets_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let shared_admin = go_minted_token(&http).await;
    let team = create_team(&http, &shared_admin, "audlc").await;

    let mut results = Vec::new();
    for (socket, side) in [
        (common::local_socket::go_socket(), "go"),
        (common::local_socket::rust_socket(), "rs"),
    ] {
        let socket = socket.expect("checked by sockets_enabled");
        let name = format!("mmrs-parity-audlc{side}");
        let local = |method: &'static str, path: String, body: String| {
            let socket = socket.clone();
            async move {
                let request = axum::http::Request::builder()
                    .method(method)
                    .uri(&path)
                    .header("Host", "localhost")
                    .header("Content-Type", "application/json")
                    .header("Content-Length", body.len().to_string())
                    .body(axum::body::Body::from(body))
                    .expect("request builds");
                let response = mm_api::local::send_over_unix(&socket, request)
                    .await
                    .unwrap_or_else(|e| panic!("{method} {path}: {e}"));
                let status = response.status().as_u16();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("body reads");
                (
                    status,
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default(),
                )
            }
        };
        let since = now_millis() - 1;
        let (status, created) = local(
            "POST",
            "/api/v4/channels".to_owned(),
            serde_json::json!({
                "team_id": team, "name": name, "display_name": "audit", "type": "O"
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, 201, "{side} created the channel");
        let channel = created["id"].as_str().expect("an id").to_owned();
        let create_rows = rows(&pool, "/api/v4/channels", since, &[], &[], &[&name]).await;
        let since = now_millis() - 1;
        let (patched, _) = local(
            "PUT",
            format!("/api/v4/channels/{channel}/patch"),
            r#"{"header":"audited"}"#.to_owned(),
        )
        .await;
        let patch_rows = rows(
            &pool,
            &format!("/api/v4/channels/{channel}/patch"),
            since,
            &[""],
            &[],
            &[],
        )
        .await;
        let since = now_millis() - 1;
        let (archived, _) = local(
            "DELETE",
            format!("/api/v4/channels/{channel}"),
            String::new(),
        )
        .await;
        let archive_rows = rows(
            &pool,
            &format!("/api/v4/channels/{channel}"),
            since,
            &[""],
            &[],
            &[],
        )
        .await;
        let subs = vec![
            (channel.clone(), "<channel>".to_owned()),
            (name.clone(), "<name>".to_owned()),
        ];
        let name_all = |found: &[Row]| found.iter().map(|r| named(r, &subs)).collect::<Vec<_>>();
        results.push(vec![
            (status, name_all(&create_rows)),
            (patched, name_all(&patch_rows)),
            (archived, name_all(&archive_rows)),
        ]);
    }
    let rust = results.pop().expect("two runs");
    let go = results.pop().expect("two runs");
    assert_eq!(
        go,
        vec![
            (
                201,
                vec![(
                    "/api/v4/channels".to_owned(),
                    "name=<name>".to_owned(),
                    String::new(),
                    String::new(),
                    String::new()
                )]
            ),
            (
                200,
                vec![(
                    "/api/v4/channels/<channel>/patch".to_owned(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new()
                )]
            ),
            (
                200,
                vec![(
                    "/api/v4/channels/<channel>".to_owned(),
                    "name=<name>".to_owned(),
                    String::new(),
                    String::new(),
                    String::new()
                )]
            ),
        ],
        "what Go wrote"
    );
    assert_eq!(rust, go);
}

/// The purge that makes Go forget a user's sessions leaves no row of its own ([D-870]): a served
/// role change clears Go's session cache for the user, and nothing lands in `Audits` beyond the
/// route's own row.
#[tokio::test]
async fn the_session_purge_is_not_audited() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let who = callers(&http, &pool, "audpg").await;
    let since = now_millis() - 1;
    let (status, _) = send(
        &http,
        RUST,
        &Call::new(
            "PUT",
            format!("/api/v4/users/{}/roles", who.plain_id),
            &who.admin,
        )
        .body(serde_json::json!({ "roles": "system_user" })),
    )
    .await;
    assert_eq!(status, 200);
    // The purge is sent after the answer's write; give it the moment it takes.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let purge_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audits WHERE createat >= $1 AND action LIKE $2")
            .bind(since)
            .bind(format!("%/users/{}/sessions/revoke%", who.plain_id))
            .fetch_one(&pool)
            .await
            .expect("the count");
    assert_eq!(purge_rows, 0, "the purge wrote an Audits row");
}

/// The `Action` is Go's `r.URL.Path` — **decoded**. A user id with one letter percent-encoded is
/// served here (the path parameter is decoded before the id check) and both servers record the
/// decoded path, not the target as sent.
#[tokio::test]
async fn an_encoded_path_is_recorded_decoded() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let who = callers(&http, &pool, "audenc").await;
    let (first, rest) = who.plain_id.split_at(1);
    let encoded = format!("/api/v4/users/%{:02X}{rest}/patch", first.as_bytes()[0]);
    let decoded = format!("/api/v4/users/{}/patch", who.plain_id);

    let mut results = Vec::new();
    for base in [GO, RUST] {
        let since = now_millis() - 1;
        let (status, _) = send(
            &http,
            base,
            &Call::new("PUT", encoded.clone(), &who.admin)
                .body(serde_json::json!({ "nickname": format!("audenc{}", base.len()) })),
        )
        .await;
        let decoded_rows = rows(&pool, &decoded, since, &who.sessions(), &[], &[]).await;
        let raw_rows = rows(&pool, &encoded, since, &who.sessions(), &[], &[]).await;
        results.push((
            status,
            decoded_rows
                .iter()
                .map(|r| named(r, &who.subs()))
                .collect::<Vec<_>>(),
            raw_rows.len(),
        ));
    }
    let rust = results.pop().expect("two runs");
    let go = results.pop().expect("two runs");
    assert_eq!(go.0, 200, "{go:?}");
    assert_eq!(go.1.len(), 1, "Go recorded the decoded path: {go:?}");
    assert_eq!(go.2, 0, "and never the target as sent");
    assert_eq!(rust, go);
}

/// A target `url.ParseRequestURI` refuses never reaches a handler on Go — `net/http` answers its
/// own `400 Bad Request` — so there is no row, and this server hands such a request to Go rather
/// than answering it from a route whose parameter would have matched.
#[tokio::test]
async fn a_target_go_cannot_parse_is_its_400_and_writes_no_row() {
    if !stack_enabled() {
        return;
    }
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let http = client();
    let who = callers(&http, &pool, "audbad").await;
    for path in [
        "/api/v4/users/username/ab%zz",
        format!("/api/v4/users/{}/patch%zz", who.plain_id).leak(),
    ] {
        let mut answers = Vec::new();
        for base in [GO, RUST] {
            let since = now_millis() - 1;
            // The username route **without a token**: its handler forwards a segment outside the
            // username charset itself, so only a request refused before the handler — the session
            // extractor's 401 — shows whether the guard handed the target to Go. A `PUT` (an
            // unregistered method) or an authenticated `GET` is forwarded either way; the
            // `unparseable-target-routed` mutation survived both versions of this test.
            let request = if path.starts_with("/api/v4/users/username/") {
                http.get(format!("{base}{path}"))
            } else {
                http.put(format!("{base}{path}"))
                    .header("Authorization", format!("Bearer {}", who.admin))
                    .json(&serde_json::json!({ "nickname": "never" }))
            };
            let response = request.send().await.expect("the server answers");
            let status = response.status().as_u16();
            let served = response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let body = response.text().await.unwrap_or_default();
            let written: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM audits WHERE createat >= $1 AND sessionid = $2",
            )
            .bind(since)
            .bind(&who.admin_session)
            .fetch_one(&pool)
            .await
            .expect("the count");
            answers.push((status, body, written));
            if base == RUST {
                assert_ne!(served.as_deref(), Some("rust"), "{path}: Go answers it");
            }
        }
        assert_eq!(answers[0].0, 400, "{path}: {answers:?}");
        assert_eq!(answers[0].2, 0, "{path}: no row");
        assert_eq!(answers[1], answers[0], "{path}");
    }
}
