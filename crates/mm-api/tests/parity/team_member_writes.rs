//! Cross-server parity for the two team-membership role writes.
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh -p mm-api --test parity team_member_write
//! ```
//!
//! # Each test gives the two servers their own team
//!
//! These routes mutate a `TeamMembers` row, so a shared fixture team would let one server's write
//! decide the other server's answer. Every test creates two teams from one template, puts one
//! plain user in both, and points one server at each — which is also what makes a byte comparison
//! meaningful, since the only field that can differ is `team_id`.
//!
//! # Everything is read back through the server that wrote it
//!
//! [D-190]: a write served by mm-api leaves Go's in-process caches stale and vice versa. Reading
//! Go's answer for a row this server wrote proves nothing about either.
//!
//! # `{"status":"OK"}` is the whole success body
//!
//! Both routes throw the updated member away, so a test that only compares response bodies cannot
//! see a wrong write at all. Hence the read-backs and the [`SocketProbe`].

use std::time::Duration;

use crate::common;

use common::{
    GO, RUST, SocketProbe, client, create_plain_user, create_team, go_minted_token,
    login_plain_user, plant_scheme, set_team_scheme, stack_enabled,
};

/// The one field two otherwise-identical answers must differ in.
fn normalise(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, inner) in map {
                if key == "team_id" {
                    out.insert(key.clone(), serde_json::json!("<team>"));
                } else {
                    out.insert(key.clone(), normalise(inner));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(normalise).collect())
        }
        other => other.clone(),
    }
}

/// One request against one server, returning `(status, body)` and asserting that a `RUST` answer
/// really came from this port rather than the proxy.
async fn call(
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: Option<&serde_json::Value>,
) -> (u16, String) {
    let mut request = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

/// A raw request with an explicit body string, for the bodies `serde_json` would refuse to build.
async fn call_raw(
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: &'static str,
) -> (u16, String) {
    let response = http
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

/// The same request against both servers, each against its own team, with the two statuses and
/// the two normalised bodies compared. Returns the Rust body so a caller can go on asserting.
async fn both(
    http: &reqwest::Client,
    token: &str,
    go_path: &str,
    rust_path: &str,
    method: reqwest::Method,
    body: Option<&serde_json::Value>,
    what: &str,
) -> String {
    let (go_status, go_raw) = call(http, GO, method.clone(), go_path, token, body).await;
    let (rust_status, rust_raw) = call(http, RUST, method, rust_path, token, body).await;
    assert_eq!(
        rust_status, go_status,
        "{what}: the status differs\n go: {go_raw}\nrust: {rust_raw}"
    );
    assert_eq!(
        go_raw.ends_with('\n'),
        rust_raw.ends_with('\n'),
        "{what}: the body's framing differs\n go: {go_raw:?}\nrust: {rust_raw:?}"
    );

    if go_status >= 400 {
        common::assert_error_bodies_match_except_known_gaps(
            go_raw.trim().as_bytes(),
            rust_raw.trim().as_bytes(),
            what,
        );
    } else {
        let go_json: serde_json::Value = serde_json::from_str(go_raw.trim()).unwrap_or_default();
        let rust_json: serde_json::Value =
            serde_json::from_str(rust_raw.trim()).unwrap_or_default();
        assert_eq!(
            normalise(&go_json),
            normalise(&rust_json),
            "{what}: the body differs\n go: {go_raw}\nrust: {rust_raw}"
        );
    }
    rust_raw
}

/// Two teams, one per server, plus a plain user who is a member of both.
struct Pair {
    go_team: String,
    rust_team: String,
    user: String,
    user_token: String,
}

impl Pair {
    fn go(&self, suffix: &str) -> String {
        format!(
            "/api/v4/teams/{}/members/{}{suffix}",
            self.go_team, self.user
        )
    }

    fn rust(&self, suffix: &str) -> String {
        format!(
            "/api/v4/teams/{}/members/{}{suffix}",
            self.rust_team, self.user
        )
    }
}

async fn pair(http: &reqwest::Client, token: &str, tag: &str) -> Pair {
    let go_team = create_team(http, token, &format!("{tag}g")).await;
    let rust_team = create_team(http, token, &format!("{tag}r")).await;
    let user = create_plain_user(http, token, &go_team, tag).await;

    let joined = http
        .post(format!("{GO}/api/v4/teams/{rust_team}/members"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"team_id": rust_team, "user_id": user.id}))
        .send()
        .await
        .expect("Go answers");
    assert!(
        joined.status().is_success(),
        "seeding the second team failed: {}",
        joined.text().await.unwrap_or_default()
    );

    Pair {
        go_team,
        rust_team,
        user: user.id,
        user_token: user.token,
    }
}

async fn cleanup(http: &reqwest::Client, token: &str, fixture: &Pair) {
    common::delete_plain_user(http, token, &fixture.user).await;
}

/// Read a membership back through **the server that wrote it**, normalised.
async fn member_on(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    team: &str,
    user: &str,
) -> serde_json::Value {
    let (status, body) = call(
        http,
        base,
        reqwest::Method::GET,
        &format!("/api/v4/teams/{team}/members/{user}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{base} could not read the member back: {body}");
    normalise(&serde_json::from_str(body.trim()).expect("a member"))
}

#[tokio::test]
async fn the_roles_update_agrees_across_every_refusal() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "tmwroles").await;
    let go = fixture.go("/roles");
    let rust = fixture.rust("/roles");

    // The success. `ReturnStatusOK` is `w.Write(MapToJSON(...))` — **no** trailing newline.
    let rust_raw = both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "team_user team_admin"})),
        "granting team_admin",
    )
    .await;
    assert_eq!(rust_raw, r#"{"status":"OK"}"#);

    // And it landed on both, read back through the server that wrote it.
    let go_member = member_on(&http, GO, &token, &fixture.go_team, &fixture.user).await;
    let rust_member = member_on(&http, RUST, &token, &fixture.rust_team, &fixture.user).await;
    assert_eq!(
        go_member, rust_member,
        "the stored membership differs after the grant"
    );
    assert_eq!(rust_member["scheme_admin"], true, "the grant did not land");
    assert_eq!(
        rust_member["roles"], "team_user team_admin",
        "the effective roles are rebuilt from the flags, not stored"
    );
    assert_eq!(
        rust_member["explicit_roles"], "",
        "a scheme-managed role must not land in explicit_roles"
    );

    // A **non**-scheme-managed role lands in `explicit_roles` and shows up in both lists. The
    // channel twin refuses a built-in role that is not channel-scoped here; the team path has no
    // such screen, which is the single biggest difference between the two files.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "team_user team_post_all"})),
        "an explicit role alongside the scheme role",
    )
    .await;
    let go_member = member_on(&http, GO, &token, &fixture.go_team, &fixture.user).await;
    let rust_member = member_on(&http, RUST, &token, &fixture.rust_team, &fixture.user).await;
    assert_eq!(go_member, rust_member, "the explicit role differs");
    assert_eq!(rust_member["explicit_roles"], "team_post_all");
    assert_eq!(rust_member["roles"], "team_post_all team_user");
    assert_eq!(
        rust_member["scheme_admin"], false,
        "the flags are set from the submitted string, not patched"
    );

    // A **system** role is refused by `IsValidUserRoles`… no: `system_admin` *is* a valid role
    // name, so it passes the handler and is refused by `GetRoleByName`'s scheme screen instead —
    // `system_admin` is scheme-managed and is not one of this team's three.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "team_user system_admin"})),
        "a system role",
    )
    .await;

    // A role name that is not an identifier at all: `IsValidUserRoles` refuses it in the
    // *handler*, with the parameter named `team_member_roles` rather than `roles`.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "team_user !"})),
        "a role name that is not an identifier",
    )
    .await;

    // A role that does not exist: `GetRoleByName`'s 404 id, restamped to a **400**.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "mmrs_no_such_role"})),
        "a role that does not exist",
    )
    .await;

    // No `roles` key at all: valid at the handler, refused in the app layer for leaving the
    // member with no base scheme role.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({})),
        "an empty roles body",
    )
    .await;

    // `team_admin` alone: the same refusal, because the flags are *set*, not patched.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "team_admin"})),
        "team_admin without team_user",
    )
    .await;

    // Promoting a member to guest: `changing_guest_role`, whose id says **`api.channel.`**
    // while describing a team. Asserted explicitly because it is the least guessable string on
    // either route.
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::PUT,
        &go,
        &token,
        Some(&serde_json::json!({"roles": "team_guest"})),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::PUT,
        &rust,
        &token,
        Some(&serde_json::json!({"roles": "team_guest"})),
    )
    .await;
    assert_eq!(go_status, 400, "Go refuses a guest promotion: {go_raw}");
    assert_eq!(rust_status, go_status, "the guest promotion status differs");
    let go_json: serde_json::Value = serde_json::from_str(&go_raw).expect("an error");
    assert_eq!(
        go_json["id"], "api.channel.update_team_member_roles.changing_guest_role.app_error",
        "the id really does say `api.channel.`: {go_raw}"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rust_raw.as_bytes(),
        "promoting a member to guest",
    );

    // `MapFromJSON` swallows a non-string value, so this is an *empty map* — the same answer as
    // an empty body, not a 400 about the type.
    let (go_status, go_raw) = call_raw(
        &http,
        GO,
        reqwest::Method::PUT,
        &go,
        &token,
        "{\"roles\": 5}",
    )
    .await;
    let (rust_status, rust_raw) = call_raw(
        &http,
        RUST,
        reqwest::Method::PUT,
        &rust,
        &token,
        "{\"roles\": 5}",
    )
    .await;
    assert_eq!(go_status, 400, "Go refuses it downstream: {go_raw}");
    assert_eq!(rust_status, go_status, "a non-string roles value differs");
    let go_json: serde_json::Value = serde_json::from_str(&go_raw).expect("an error");
    assert_eq!(
        go_json["id"], "api.team.update_team_member_roles.unset_user_scheme.app_error",
        "the value was dropped, not rejected: {go_raw}"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rust_raw.as_bytes(),
        "a non-string roles value",
    );

    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn the_scheme_roles_update_takes_only_two_bodies() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "tmwscheme").await;
    let go = fixture.go("/schemeRoles");
    let rust = fixture.rust("/schemeRoles");

    // `scheme_user: true, scheme_admin: true` — one of the only two accepted bodies.
    let rust_raw = both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({
            "scheme_guest": false, "scheme_user": true, "scheme_admin": true
        })),
        "promoting to team admin",
    )
    .await;
    assert_eq!(rust_raw, r#"{"status":"OK"}"#);

    let go_member = member_on(&http, GO, &token, &fixture.go_team, &fixture.user).await;
    let rust_member = member_on(&http, RUST, &token, &fixture.rust_team, &fixture.user).await;
    assert_eq!(go_member, rust_member, "the promotion differs");
    assert_eq!(rust_member["scheme_admin"], true);
    assert_eq!(rust_member["roles"], "team_user team_admin");

    // And back down — the other accepted body.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({
            "scheme_guest": false, "scheme_user": true, "scheme_admin": false
        })),
        "demoting to team member",
    )
    .await;
    let go_member = member_on(&http, GO, &token, &fixture.go_team, &fixture.user).await;
    let rust_member = member_on(&http, RUST, &token, &fixture.rust_team, &fixture.user).await;
    assert_eq!(go_member, rust_member, "the demotion differs");
    assert_eq!(rust_member["scheme_admin"], false);

    // `scheme_guest: true` → `user_and_guest`, an id the `/roles` route never produces.
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::PUT,
        &go,
        &token,
        Some(&serde_json::json!({
            "scheme_guest": true, "scheme_user": true, "scheme_admin": false
        })),
    )
    .await;
    assert_eq!(go_status, 400, "Go refuses a guest request: {go_raw}");
    let go_json: serde_json::Value = serde_json::from_str(&go_raw).expect("an error");
    assert_eq!(
        go_json["id"], "api.team.update_team_member_roles.user_and_guest.app_error",
        "the guest refusal's id: {go_raw}"
    );
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({
            "scheme_guest": true, "scheme_user": true, "scheme_admin": false
        })),
        "asking for scheme_guest",
    )
    .await;

    // `scheme_user: false` → `unset_user_scheme`, which is also what `{}` decodes to.
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({
            "scheme_guest": false, "scheme_user": false, "scheme_admin": true
        })),
        "asking to unset scheme_user",
    )
    .await;
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::PUT,
        Some(&serde_json::json!({})),
        "an empty scheme_roles body",
    )
    .await;

    // Unlike `/roles`, this route **does** 400 on a body that is not an object — the decoder's
    // error rather than an empty map.
    let (go_status, go_raw) =
        call_raw(&http, GO, reqwest::Method::PUT, &go, &token, "not json").await;
    let (rust_status, rust_raw) =
        call_raw(&http, RUST, reqwest::Method::PUT, &rust, &token, "not json").await;
    assert_eq!(go_status, 400, "Go refuses a malformed body: {go_raw}");
    assert_eq!(rust_status, go_status, "the malformed-body status differs");
    common::assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rust_raw.as_bytes(),
        "a malformed scheme_roles body",
    );

    // `/schemeroles` is not the route — gorilla and axum both match the segment literally, so the
    // lowercase spelling reaches neither handler and Go answers for both.
    let lower_go = go.replace("/schemeRoles", "/schemeroles");
    let lower_rust = rust.replace("/schemeRoles", "/schemeroles");
    let (go_status, _) = call(
        &http,
        GO,
        reqwest::Method::PUT,
        &lower_go,
        &token,
        Some(&serde_json::json!({"scheme_user": true})),
    )
    .await;
    let (rust_status, _) = call_forwarded(
        &http,
        reqwest::Method::PUT,
        &lower_rust,
        &token,
        Some(&serde_json::json!({"scheme_user": true})),
    )
    .await;
    assert_eq!(
        rust_status, go_status,
        "the lowercase spelling must reach Go on both"
    );

    cleanup(&http, &token, &fixture).await;
}

/// A request to this server that is expected to be **forwarded**, so the `x-mmrs-served-by`
/// assertion must not run.
async fn call_forwarded(
    http: &reqwest::Client,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: Option<&serde_json::Value>,
) -> (u16, String) {
    let mut request = http
        .request(method, format!("{RUST}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().await.expect("this server answers");
    let status = response.status().as_u16();
    (status, response.text().await.expect("a body"))
}

#[tokio::test]
async fn a_caller_without_manage_team_roles_is_refused_after_the_body_check() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "tmwperm").await;

    // A fresh token: `SessionHasPermissionTo` reads the roles copied onto the session at login.
    let plain = login_plain_user(&http, "tmwperm").await;

    // A plain team member holds neither `manage_team_roles` — 403 on both routes.
    for suffix in ["/roles", "/schemeRoles"] {
        both(
            &http,
            &plain,
            &fixture.go(suffix),
            &fixture.rust(suffix),
            reqwest::Method::PUT,
            Some(&serde_json::json!({
                "roles": "team_user", "scheme_user": true, "scheme_admin": false
            })),
            &format!("a plain user on {suffix}"),
        )
        .await;
    }

    // **The body check runs first on `/roles`.** The same caller sending an invalid role string
    // gets a 400, not the 403 above — which is the check order, and it is only observable
    // through a caller who fails both gates.
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::PUT,
        &fixture.go("/roles"),
        &plain,
        Some(&serde_json::json!({"roles": "!"})),
    )
    .await;
    assert_eq!(
        go_status, 400,
        "Go validates the body before the permission: {go_raw}"
    );
    both(
        &http,
        &plain,
        &fixture.go("/roles"),
        &fixture.rust("/roles"),
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "!"})),
        "a plain user sending an invalid role",
    )
    .await;

    // **`/schemeRoles` checks the body first too**, and its body check is the decoder.
    let (go_status, _) = call_raw(
        &http,
        GO,
        reqwest::Method::PUT,
        &fixture.go("/schemeRoles"),
        &plain,
        "not json",
    )
    .await;
    assert_eq!(go_status, 400, "the decoder runs before the permission");
    let (rust_status, _) = call_raw(
        &http,
        RUST,
        reqwest::Method::PUT,
        &fixture.rust("/schemeRoles"),
        &plain,
        "not json",
    )
    .await;
    assert_eq!(rust_status, go_status, "the check order differs");

    // A malformed id in either segment is a 400 before anything else, and the **team** id wins.
    both(
        &http,
        &token,
        &format!("/api/v4/teams/{}/members/nope/roles", fixture.go_team),
        &format!("/api/v4/teams/{}/members/nope/roles", fixture.rust_team),
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "team_user"})),
        "a malformed user id",
    )
    .await;

    // A well-formed id that is not a member: the store finds no row, and Go's `UpdateMember`
    // writes nothing yet reports no miss — so this is **200**, not 404. The least intuitive
    // thing on either route.
    let ghost = "y9i4er48tt8bukijy7i3u5y9ar";
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::PUT,
        &format!("/api/v4/teams/{}/members/{ghost}/roles", fixture.go_team),
        &token,
        Some(&serde_json::json!({"roles": "team_user"})),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::PUT,
        &format!("/api/v4/teams/{}/members/{ghost}/roles", fixture.rust_team),
        &token,
        Some(&serde_json::json!({"roles": "team_user"})),
    )
    .await;
    assert_eq!(
        rust_status, go_status,
        "a non-member differs\n go: {go_raw}\nrust: {rust_raw}"
    );
    if go_status >= 400 {
        common::assert_error_bodies_match_except_known_gaps(
            go_raw.trim().as_bytes(),
            rust_raw.trim().as_bytes(),
            "a non-member",
        );
    } else {
        assert_eq!(go_raw, rust_raw, "a non-member's body differs");
    }

    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn a_team_scheme_replaces_the_three_role_names() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "tmwsch2").await;

    let Some(scheme) = plant_scheme("team", "tmwsch2").await else {
        return;
    };
    assert!(set_team_scheme(&fixture.go_team, Some(&scheme)).await);
    assert!(set_team_scheme(&fixture.rust_team, Some(&scheme)).await);

    // The planted scheme's three `defaultteam*role` columns are empty strings, so
    // `GetSchemeRolesForTeam` answers `("", "", "")` and **no** submitted scheme-managed name
    // matches. `team_user` is therefore refused as "not part of this team's scheme" rather than
    // accepted — which is the branch that proves the scheme lookup happened at all.
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::PUT,
        &fixture.go("/roles"),
        &token,
        Some(&serde_json::json!({"roles": "team_user"})),
    )
    .await;
    assert_eq!(
        go_status, 400,
        "an empty scheme role name matches nothing: {go_raw}"
    );
    let go_json: serde_json::Value = serde_json::from_str(&go_raw).expect("an error");
    assert_eq!(
        go_json["id"], "api.channel.update_team_member_roles.scheme_role.app_error",
        "and this id says `api.channel.` too: {go_raw}"
    );
    both(
        &http,
        &token,
        &fixture.go("/roles"),
        &fixture.rust("/roles"),
        reqwest::Method::PUT,
        Some(&serde_json::json!({"roles": "team_user"})),
        "a scheme-managed role under a scheme that does not name it",
    )
    .await;

    // The store's fallback is *not* the app's: `getTeamRoles` substitutes the three constants for
    // an empty scheme column, so the membership still reads back as `team_user`. Both servers.
    let go_member = member_on(&http, GO, &token, &fixture.go_team, &fixture.user).await;
    let rust_member = member_on(&http, RUST, &token, &fixture.rust_team, &fixture.user).await;
    assert_eq!(go_member, rust_member, "the stored membership differs");
    assert_eq!(
        rust_member["roles"], "team_user",
        "the read path falls back to the constants where the write path did not"
    );

    // `/schemeRoles` does not consult the scheme at all, so it still works under one.
    both(
        &http,
        &token,
        &fixture.go("/schemeRoles"),
        &fixture.rust("/schemeRoles"),
        reqwest::Method::PUT,
        Some(&serde_json::json!({
            "scheme_guest": false, "scheme_user": true, "scheme_admin": true
        })),
        "schemeRoles under a team scheme",
    )
    .await;
    let go_member = member_on(&http, GO, &token, &fixture.go_team, &fixture.user).await;
    let rust_member = member_on(&http, RUST, &token, &fixture.rust_team, &fixture.user).await;
    assert_eq!(go_member, rust_member, "the scheme promotion differs");
    assert_eq!(rust_member["scheme_admin"], true);

    let _ = set_team_scheme(&fixture.go_team, None).await;
    let _ = set_team_scheme(&fixture.rust_team, None).await;
    cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn the_role_writes_publish_memberrole_updated() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = pair(&http, &token, "tmwevent").await;

    // The event is addressed to the **member**, so the probe has to be the member's socket, not
    // the admin's. A port that broadcast to the team instead would pass an admin-socket test.
    let mut go_socket = SocketProbe::connect(GO, &fixture.user_token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &fixture.user_token).await;

    for (base, path) in [(GO, fixture.go("/roles")), (RUST, fixture.rust("/roles"))] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::PUT,
            &path,
            &token,
            Some(&serde_json::json!({"roles": "team_user team_admin"})),
        )
        .await;
        assert_eq!(status, 200, "{base} granted the role: {body}");
    }
    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_events = memberrole_events(&go_socket);
    let rust_events = memberrole_events(&rust_socket);
    assert_eq!(
        go_events.len(),
        1,
        "Go published one memberrole_updated: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_events.len(),
        go_events.len(),
        "the memberrole_updated count differs: {:?}",
        rust_socket.raw
    );

    // Addressed to the user and to **no team and no channel** — a port that filled the team id
    // in would broadcast one member's roles to everybody on the team.
    assert_eq!(
        rust_events[0]["broadcast"]["user_id"],
        fixture.user.as_str()
    );
    assert_eq!(rust_events[0]["broadcast"]["team_id"], "");
    assert_eq!(rust_events[0]["broadcast"]["channel_id"], "");
    assert_eq!(
        go_events[0]["broadcast"], rust_events[0]["broadcast"],
        "the memberrole_updated broadcast differs"
    );

    // The payload is the member **as a JSON string** under `member`, not as an object and not
    // under `teamMember`.
    let go_payload = go_events[0]["data"]["member"]
        .as_str()
        .expect("Go sends a JSON string");
    let rust_payload = rust_events[0]["data"]["member"]
        .as_str()
        .expect("a JSON string in `data.member`");
    let go_member: serde_json::Value = serde_json::from_str(go_payload).expect("the member parses");
    let rust_member: serde_json::Value =
        serde_json::from_str(rust_payload).expect("the member parses");
    assert_eq!(
        normalise(&go_member),
        normalise(&rust_member),
        "the published member differs\n go: {go_payload}\nrust: {rust_payload}"
    );
    assert_eq!(rust_member["scheme_admin"], true);
    assert_eq!(rust_member["roles"], "team_user team_admin");
    assert!(
        rust_member.get("create_at").is_none(),
        "`CreateAt` is `json:\"-\"` and must not reach the socket: {rust_payload}"
    );

    // `/schemeRoles` publishes the same event.
    for (base, path) in [
        (GO, fixture.go("/schemeRoles")),
        (RUST, fixture.rust("/schemeRoles")),
    ] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::PUT,
            &path,
            &token,
            Some(&serde_json::json!({
                "scheme_guest": false, "scheme_user": true, "scheme_admin": false
            })),
        )
        .await;
        assert_eq!(status, 200, "{base} demoted the member: {body}");
    }
    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_events = memberrole_events(&go_socket);
    let rust_events = memberrole_events(&rust_socket);
    assert_eq!(
        go_events.len(),
        2,
        "the second write published a second event: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_events.len(),
        go_events.len(),
        "the second memberrole_updated count differs: {:?}",
        rust_socket.raw
    );
    let rust_member: serde_json::Value = serde_json::from_str(
        rust_events[1]["data"]["member"]
            .as_str()
            .expect("a JSON string"),
    )
    .expect("the member parses");
    assert_eq!(
        rust_member["scheme_admin"], false,
        "the published member is the one just written"
    );

    cleanup(&http, &token, &fixture).await;
}

/// Every `memberrole_updated` the probe saw, in arrival order.
fn memberrole_events(probe: &SocketProbe) -> Vec<serde_json::Value> {
    probe.events_named("memberrole_updated")
}
