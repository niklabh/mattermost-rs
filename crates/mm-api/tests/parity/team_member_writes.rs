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

/// The one field two otherwise-identical answers must differ in — plus the `message` of any
/// **nested** `AppError`, which the top-level comparison helper already excuses.
///
/// The graceful batch body embeds an `AppError` per failed user, and this port has no i18n
/// catalogue: Go answers `"Unable to find the user."` where we answer the id. That is the
/// standing divergence recorded as D-092; it is masked here rather than skipped, so the `id`,
/// `status_code` and `detailed_error` of every nested error are still compared.
fn normalise(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let is_app_error = map.contains_key("id") && map.contains_key("status_code");
            let mut out = serde_json::Map::new();
            for (key, inner) in map {
                if key == "team_id" {
                    out.insert(key.clone(), serde_json::json!("<team>"));
                } else if key == "message" && is_app_error {
                    out.insert(key.clone(), serde_json::json!("<untranslated, D-092>"));
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

    // **The bystander.** The caller is the team's creator and therefore its admin; the write
    // above must not have touched their row. Without `AND userid = $2` the UPDATE rewrites every
    // member of the team, and the *target's* read-back looks perfectly correct while the admin
    // has silently lost `team_admin` and gained `team_post_all`.
    let me = common::logged_in_user_id();
    for (base, team) in [(GO, &fixture.go_team), (RUST, &fixture.rust_team)] {
        let admin = member_on(&http, base, &token, team, me).await;
        assert_eq!(
            admin["roles"], "team_user team_admin",
            "{base} rewrote a bystander's roles: {admin}"
        );
        assert_eq!(admin["explicit_roles"], "", "{base} rewrote a bystander");
    }

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

/// Every event of one type whose **payload** names this test's team, in arrival order.
///
/// The admin's socket belongs to a user who is in many channels on many teams, so it sees the
/// `user_added` traffic of every other suite running in the same binary. Counting all of them made
/// this test fail under `--workspace` and pass alone — the documented flake class, and here it was
/// the test's fault rather than the harness's. Both events carry `team_id` in `data`, which is the
/// one field that separates this fixture's traffic from everybody else's.
fn events_for_team(probe: &SocketProbe, event_type: &str, team_id: &str) -> Vec<serde_json::Value> {
    probe
        .events_named(event_type)
        .into_iter()
        .filter(|frame| frame["data"]["team_id"] == team_id)
        .collect()
}

/// Every `memberrole_updated` the probe saw, in arrival order.
fn memberrole_events(probe: &SocketProbe) -> Vec<serde_json::Value> {
    probe.events_named("memberrole_updated")
}

/// Two teams nobody has joined yet, plus a plain user parked on a third.
///
/// The add routes need a user who is **not** a member, and `create_plain_user` always joins the
/// team it is given — hence the third "home" team, which nothing else in the test touches.
struct AddPair {
    go_team: String,
    rust_team: String,
    user: String,
    user_token: String,
}

async fn add_pair(http: &reqwest::Client, token: &str, tag: &str) -> AddPair {
    let home = create_team(http, token, &format!("{tag}h")).await;
    let go_team = create_team(http, token, &format!("{tag}g")).await;
    let rust_team = create_team(http, token, &format!("{tag}r")).await;
    let user = create_plain_user(http, token, &home, tag).await;
    AddPair {
        go_team,
        rust_team,
        user: user.id,
        user_token: user.token,
    }
}

async fn add_cleanup(http: &reqwest::Client, token: &str, fixture: &AddPair) {
    common::delete_plain_user(http, token, &fixture.user).await;
}

/// The channel named `name` on `team`, read through `base`.
async fn channel_named(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    team: &str,
    name: &str,
) -> serde_json::Value {
    let (status, body) = call(
        http,
        base,
        reqwest::Method::GET,
        &format!("/api/v4/teams/{team}/channels/name/{name}"),
        token,
        None,
    )
    .await;
    assert_eq!(status, 200, "{base} has no {name} on {team}: {body}");
    serde_json::from_str(body.trim()).expect("a channel")
}

#[tokio::test]
async fn adding_a_member_agrees_and_joins_the_default_channels() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = add_pair(&http, &token, "tmwadd").await;
    let user = &fixture.user;

    let go = format!("/api/v4/teams/{}/members", fixture.go_team);
    let rust = format!("/api/v4/teams/{}/members", fixture.rust_team);

    // The body carries the team id and it has to match the path's.
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &go,
        &token,
        Some(&serde_json::json!({"team_id": fixture.go_team, "user_id": user})),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &rust,
        &token,
        Some(&serde_json::json!({"team_id": fixture.rust_team, "user_id": user})),
    )
    .await;
    assert_eq!(go_status, 201, "Go added the member: {go_raw}");
    assert_eq!(rust_status, go_status, "the add status differs: {rust_raw}");
    assert_eq!(
        go_raw.ends_with('\n'),
        rust_raw.ends_with('\n'),
        "`json.NewEncoder(w).Encode` frames with a newline\n go: {go_raw:?}\nrust: {rust_raw:?}"
    );
    assert!(
        rust_raw.ends_with('\n'),
        "and the framing is the newline one"
    );
    let go_member: serde_json::Value = serde_json::from_str(go_raw.trim()).expect("a member");
    let rust_member: serde_json::Value = serde_json::from_str(rust_raw.trim()).expect("a member");
    assert_eq!(
        normalise(&go_member),
        normalise(&rust_member),
        "the created member differs\n go: {go_raw}\nrust: {rust_raw}"
    );
    // The caller holds `manage_team_roles` on a team it created, so nothing is sanitised and the
    // roles are the real ones.
    assert_eq!(rust_member["roles"], "team_user");
    assert_eq!(rust_member["scheme_user"], true);
    assert_eq!(rust_member["scheme_admin"], false);
    assert_eq!(rust_member["delete_at"], 0);
    assert!(
        rust_member.get("create_at").is_none(),
        "`CreateAt` is `json:\"-\"`: {rust_raw}"
    );

    // **A second add is idempotent** — 201 with the stored member, no write and no event.
    let (go_status, go_again) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &go,
        &token,
        Some(&serde_json::json!({"team_id": fixture.go_team, "user_id": user})),
    )
    .await;
    let (rust_status, rust_again) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &rust,
        &token,
        Some(&serde_json::json!({"team_id": fixture.rust_team, "user_id": user})),
    )
    .await;
    assert_eq!(go_status, 201, "a re-add is still Created: {go_again}");
    assert_eq!(rust_status, go_status, "the re-add status differs");
    assert_eq!(
        normalise(&serde_json::from_str(go_again.trim()).expect("a member")),
        normalise(&serde_json::from_str(rust_again.trim()).expect("a member")),
        "the re-add answer differs\n go: {go_again}\nrust: {rust_again}"
    );

    // **The membership landed**, read back through the server that wrote it.
    let go_stored = member_on(&http, GO, &token, &fixture.go_team, user).await;
    let rust_stored = member_on(&http, RUST, &token, &fixture.rust_team, user).await;
    assert_eq!(go_stored, rust_stored, "the stored membership differs");

    // **And so did the default channels.** `JoinDefaultChannels` is a soft error inside the join,
    // so nothing in the response body would show this going wrong.
    for name in ["town-square", "off-topic"] {
        let go_channel = channel_named(&http, GO, &token, &fixture.go_team, name).await;
        let rust_channel = channel_named(&http, RUST, &token, &fixture.rust_team, name).await;
        let go_id = go_channel["id"].as_str().expect("an id");
        let rust_id = rust_channel["id"].as_str().expect("an id");

        let (go_status, go_body) = call(
            &http,
            GO,
            reqwest::Method::GET,
            &format!("/api/v4/channels/{go_id}/members/{user}"),
            &token,
            None,
        )
        .await;
        let (rust_status, rust_body) = call(
            &http,
            RUST,
            reqwest::Method::GET,
            &format!("/api/v4/channels/{rust_id}/members/{user}"),
            &token,
            None,
        )
        .await;
        assert_eq!(go_status, 200, "Go put the user in {name}: {go_body}");
        assert_eq!(
            rust_status, go_status,
            "this server did not put the user in {name}: {rust_body}"
        );
        let strip = |raw: &str, channel: &str| -> serde_json::Value {
            let mut value: serde_json::Value =
                serde_json::from_str(raw.trim()).expect("a channel member");
            if let Some(object) = value.as_object_mut() {
                object.insert("channel_id".to_owned(), serde_json::json!("<channel>"));
                // `LastUpdateAt` is `GetMillis()` at save time and the two saves are seconds
                // apart; `MsgCount` follows the channel's own post count, which Go's join system
                // post moves and this port's does not (D-243).
                for key in ["last_update_at", "msg_count", "msg_count_root"] {
                    object.insert(key.to_owned(), serde_json::json!("<moves>"));
                }
            }
            let _ = channel;
            value
        };
        assert_eq!(
            strip(&go_body, go_id),
            strip(&rust_body, rust_id),
            "the {name} membership differs\n go: {go_body}\nrust: {rust_body}"
        );
    }

    // **The join history rows.** `LogJoinEvent(userID, channelID, ...)` is invisible in every
    // response body, and its two id arguments are adjacent strings — swapping them writes a row
    // nothing will ever match. Read straight from the table, which is the only place it shows.
    for (team, name) in [
        (&fixture.go_team, "town-square"),
        (&fixture.rust_team, "town-square"),
    ] {
        let base = if team == &fixture.go_team { GO } else { RUST };
        let channel = channel_named(&http, base, &token, team, name).await;
        let channel_id = channel["id"].as_str().expect("an id");
        let rows = common::channel_member_history(channel_id, user)
            .await
            .expect("the history table is readable");
        assert_eq!(
            rows.len(),
            1,
            "{base} wrote {} join rows for {name}",
            rows.len()
        );
        assert!(rows[0].0 > 0, "{base} wrote a zero join time");
        assert!(rows[0].1.is_none(), "{base} closed the stay immediately");
    }

    // **The initial sidebar categories were created for the right pair of ids.** The read below
    // cannot prove this on its own — `GET …/channels/categories` creates them itself when it
    // finds none, so it self-heals a join that wrote them under swapped ids. The table is the
    // only place the difference shows, and the *absence* of rows for the swapped pair is the
    // assertion that catches it.
    for (base, team) in [(GO, &fixture.go_team), (RUST, &fixture.rust_team)] {
        let owned = common::sidebar_category_ids(user, team)
            .await
            .expect("the sidebar table is readable");
        assert_eq!(
            owned.len(),
            3,
            "{base} did not create the three initial categories: {owned:?}"
        );
        let swapped = common::sidebar_category_ids(team, user)
            .await
            .expect("the sidebar table is readable");
        assert!(
            swapped.is_empty(),
            "{base} created categories under swapped ids: {swapped:?}"
        );
    }

    // **The initial sidebar categories were created**, with Go's deterministic ids.
    for (base, team) in [(GO, &fixture.go_team), (RUST, &fixture.rust_team)] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::GET,
            &format!("/api/v4/users/{user}/teams/{team}/channels/categories"),
            &token,
            None,
        )
        .await;
        assert_eq!(status, 200, "{base} has no categories: {body}");
        let categories: serde_json::Value = serde_json::from_str(body.trim()).expect("categories");
        let order = categories["order"]
            .as_array()
            .expect("an order")
            .iter()
            .map(|id| id.as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![
                format!("favorites_{user}_{team}"),
                format!("channels_{user}_{team}"),
                format!("direct_messages_{user}_{team}"),
            ],
            "{base} built the wrong initial categories: {body}"
        );
    }

    add_cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn the_add_bodys_refusals_agree() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = add_pair(&http, &token, "tmwaddbad").await;

    let go = format!("/api/v4/teams/{}/members", fixture.go_team);
    let rust = format!("/api/v4/teams/{}/members", fixture.rust_team);

    // A body that is not a `TeamMember` at all: the route's **own** id, not the shared
    // `invalid_body_param` one.
    let (go_status, go_raw) = call_raw(&http, GO, reqwest::Method::POST, &go, &token, "[]").await;
    let (rust_status, rust_raw) =
        call_raw(&http, RUST, reqwest::Method::POST, &rust, &token, "[]").await;
    assert_eq!(go_status, 400, "Go refuses a list here: {go_raw}");
    assert_eq!(rust_status, go_status, "the malformed-body status differs");
    let go_json: serde_json::Value = serde_json::from_str(&go_raw).expect("an error");
    assert_eq!(
        go_json["id"], "api.team.add_team_member.invalid_body.app_error",
        "the route has its own id: {go_raw}"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rust_raw.as_bytes(),
        "a malformed add body",
    );

    // A `team_id` that does not match the path: `invalid_body_param` naming `team_id`, which is
    // a *different* id from the malformed path segment's `invalid_url_param`.
    let mismatched = serde_json::json!({
        "team_id": "y9i4er48tt8bukijy7i3u5y9ar", "user_id": fixture.user
    });
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &go,
        &token,
        Some(&mismatched),
    )
    .await;
    assert_eq!(go_status, 400, "Go refuses a mismatched team_id: {go_raw}");
    let go_json: serde_json::Value = serde_json::from_str(&go_raw).expect("an error");
    assert_eq!(go_json["id"], "api.context.invalid_body_param.app_error");
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::POST,
        Some(&mismatched),
        "a team_id that does not match the path",
    )
    .await;

    // A `user_id` that is not an id.
    for bad in ["", "me", "short"] {
        let go_body = serde_json::json!({"team_id": fixture.go_team, "user_id": bad});
        let rust_body = serde_json::json!({"team_id": fixture.rust_team, "user_id": bad});
        let (go_status, go_raw) = call(
            &http,
            GO,
            reqwest::Method::POST,
            &go,
            &token,
            Some(&go_body),
        )
        .await;
        let (rust_status, rust_raw) = call(
            &http,
            RUST,
            reqwest::Method::POST,
            &rust,
            &token,
            Some(&rust_body),
        )
        .await;
        assert_eq!(go_status, 400, "Go refuses user_id={bad:?}: {go_raw}");
        assert_eq!(rust_status, go_status, "user_id={bad:?} differs");
        common::assert_error_bodies_match_except_known_gaps(
            go_raw.as_bytes(),
            rust_raw.as_bytes(),
            "an invalid user_id",
        );
    }

    // A well-formed user id that names nobody: the *store* misses, and the id is the
    // `MissingAccountError` constant rather than a translation key.
    let ghost =
        serde_json::json!({"team_id": fixture.go_team, "user_id": "y9i4er48tt8bukijy7i3u5y9ar"});
    let ghost_rust =
        serde_json::json!({"team_id": fixture.rust_team, "user_id": "y9i4er48tt8bukijy7i3u5y9ar"});
    let (go_status, go_raw) =
        call(&http, GO, reqwest::Method::POST, &go, &token, Some(&ghost)).await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &rust,
        &token,
        Some(&ghost_rust),
    )
    .await;
    assert_eq!(
        go_status, 404,
        "Go cannot add a user that is not there: {go_raw}"
    );
    assert_eq!(rust_status, go_status, "a missing user differs: {rust_raw}");
    common::assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rust_raw.as_bytes(),
        "a user that does not exist",
    );

    // A team that does not exist: the *path* is valid, so this is the app layer's 404.
    let missing_team = "y9i4er48tt8bukijy7i3u5y9ar";
    let body = serde_json::json!({"team_id": missing_team, "user_id": fixture.user});
    both(
        &http,
        &token,
        &format!("/api/v4/teams/{missing_team}/members"),
        &format!("/api/v4/teams/{missing_team}/members"),
        reqwest::Method::POST,
        Some(&body),
        "a team that does not exist",
    )
    .await;

    add_cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn the_batch_route_agrees_in_both_framings() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = add_pair(&http, &token, "tmwbatch").await;
    let user = &fixture.user;

    let go = format!("/api/v4/teams/{}/members/batch", fixture.go_team);
    let rust = format!("/api/v4/teams/{}/members/batch", fixture.rust_team);

    // An empty list and an oversized one, both refused before anything is read.
    for (body, what) in [
        (serde_json::json!([]), "an empty batch"),
        (
            serde_json::Value::Array(
                (0..257)
                    .map(|_| serde_json::json!({"team_id": fixture.go_team, "user_id": user}))
                    .collect(),
            ),
            "an oversized batch",
        ),
    ] {
        let (go_status, go_raw) =
            call(&http, GO, reqwest::Method::POST, &go, &token, Some(&body)).await;
        let (rust_status, rust_raw) = call(
            &http,
            RUST,
            reqwest::Method::POST,
            &rust,
            &token,
            Some(&body),
        )
        .await;
        assert_eq!(go_status, 400, "Go refuses {what}: {go_raw}");
        assert_eq!(rust_status, go_status, "{what} differs: {rust_raw}");
        common::assert_error_bodies_match_except_known_gaps(
            go_raw.as_bytes(),
            rust_raw.as_bytes(),
            what,
        );
    }

    // The plain (non-graceful) form: a bare list of members, **written not encoded** so there is
    // no trailing newline.
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &go,
        &token,
        Some(&serde_json::json!([{"team_id": fixture.go_team, "user_id": user}])),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &rust,
        &token,
        Some(&serde_json::json!([{"team_id": fixture.rust_team, "user_id": user}])),
    )
    .await;
    assert_eq!(go_status, 201, "Go added the batch: {go_raw}");
    assert_eq!(
        rust_status, go_status,
        "the batch status differs: {rust_raw}"
    );
    assert!(
        !go_raw.ends_with('\n'),
        "`w.Write(js)` leaves no newline: {go_raw:?}"
    );
    assert_eq!(
        go_raw.ends_with('\n'),
        rust_raw.ends_with('\n'),
        "the batch framing differs\n go: {go_raw:?}\nrust: {rust_raw:?}"
    );
    let go_list: serde_json::Value = serde_json::from_str(&go_raw).expect("a list");
    let rust_list: serde_json::Value = serde_json::from_str(&rust_raw).expect("a list");
    assert!(
        go_list.is_array(),
        "the plain form is a bare array: {go_raw}"
    );
    assert_eq!(
        normalise(&go_list),
        normalise(&rust_list),
        "the batch body differs\n go: {go_raw}\nrust: {rust_raw}"
    );
    assert!(
        go_list[0].get("user_id").is_some() && go_list[0].get("member").is_none(),
        "the plain form holds members, not wrappers: {go_raw}"
    );

    // The **graceful** form: a list of `{user_id, member, error}`, with `error: null` on success.
    // Any non-empty value turns it on, `0` included.
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &format!("{go}?graceful=0"),
        &token,
        Some(&serde_json::json!([{"team_id": fixture.go_team, "user_id": user}])),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &format!("{rust}?graceful=0"),
        &token,
        Some(&serde_json::json!([{"team_id": fixture.rust_team, "user_id": user}])),
    )
    .await;
    assert_eq!(go_status, 201, "Go answered the graceful batch: {go_raw}");
    assert_eq!(
        rust_status, go_status,
        "the graceful status differs: {rust_raw}"
    );
    let go_list: serde_json::Value = serde_json::from_str(&go_raw).expect("a list");
    let rust_list: serde_json::Value = serde_json::from_str(&rust_raw).expect("a list");
    assert_eq!(
        normalise(&go_list),
        normalise(&rust_list),
        "the graceful body differs\n go: {go_raw}\nrust: {rust_raw}"
    );
    assert!(
        go_list[0]["error"].is_null(),
        "a success carries `error: null`, not an absent key: {go_raw}"
    );
    assert!(
        go_list[0].get("error").is_some(),
        "and the key is present: {go_raw}"
    );
    assert_eq!(go_list[0]["user_id"], user.as_str());
    assert_eq!(go_list[0]["member"]["roles"], "team_user");

    // A **partly refused** graceful batch: one real user and one that does not exist. The good
    // one is added and the bad one carries an error, in the order submitted.
    let ghost = "y9i4er48tt8bukijy7i3u5y9ar";
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &format!("{go}?graceful=1"),
        &token,
        Some(&serde_json::json!([
            {"team_id": fixture.go_team, "user_id": user},
            {"team_id": fixture.go_team, "user_id": ghost},
        ])),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &format!("{rust}?graceful=1"),
        &token,
        Some(&serde_json::json!([
            {"team_id": fixture.rust_team, "user_id": user},
            {"team_id": fixture.rust_team, "user_id": ghost},
        ])),
    )
    .await;
    assert_eq!(
        rust_status, go_status,
        "the partly-refused status differs\n go: {go_raw}\nrust: {rust_raw}"
    );
    let go_list: serde_json::Value = serde_json::from_str(&go_raw).expect("a list");
    let rust_list: serde_json::Value = serde_json::from_str(&rust_raw).expect("a list");
    assert_eq!(
        normalise(&go_list),
        normalise(&rust_list),
        "the partly-refused body differs\n go: {go_raw}\nrust: {rust_raw}"
    );

    // And the same pair **without** `graceful`: the whole request is the first error, and the
    // users before it stay added.
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &go,
        &token,
        Some(&serde_json::json!([
            {"team_id": fixture.go_team, "user_id": ghost},
            {"team_id": fixture.go_team, "user_id": user},
        ])),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &rust,
        &token,
        Some(&serde_json::json!([
            {"team_id": fixture.rust_team, "user_id": ghost},
            {"team_id": fixture.rust_team, "user_id": user},
        ])),
    )
    .await;
    assert!(go_status >= 400, "Go fails the whole batch: {go_raw}");
    assert_eq!(
        rust_status, go_status,
        "the non-graceful failure differs\n go: {go_raw}\nrust: {rust_raw}"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rust_raw.as_bytes(),
        "a non-graceful batch with a bad id",
    );

    // A member whose `team_id` names another team: refused with the *user id* in the parameter
    // name, which is the least guessable error string on either add route.
    let wrong_team =
        serde_json::json!([{"team_id": "y9i4er48tt8bukijy7i3u5y9ar", "user_id": user}]);
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &go,
        &token,
        Some(&wrong_team),
    )
    .await;
    assert_eq!(go_status, 400, "Go refuses a foreign team_id: {go_raw}");
    both(
        &http,
        &token,
        &go,
        &rust,
        reqwest::Method::POST,
        Some(&wrong_team),
        "a batch member naming another team",
    )
    .await;

    add_cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn the_add_permission_matrix_turns_on_who_is_added() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = add_pair(&http, &token, "tmwaddperm").await;
    let plain = login_plain_user(&http, "tmwaddperm").await;

    // **A self-join to a team with `AllowOpenInvite` off** needs the *system* permission
    // `join_private_teams`, which a plain user does not have. Teams are created with
    // `allow_open_invite` false.
    let self_body_go = serde_json::json!({"team_id": fixture.go_team, "user_id": fixture.user});
    let self_body_rust = serde_json::json!({"team_id": fixture.rust_team, "user_id": fixture.user});
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &format!("/api/v4/teams/{}/members", fixture.go_team),
        &plain,
        Some(&self_body_go),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &format!("/api/v4/teams/{}/members", fixture.rust_team),
        &plain,
        Some(&self_body_rust),
    )
    .await;
    assert_eq!(go_status, 403, "Go refuses a private self-join: {go_raw}");
    assert_eq!(
        rust_status, go_status,
        "the self-join refusal differs: {rust_raw}"
    );
    common::assert_error_bodies_match_except_known_gaps(
        go_raw.as_bytes(),
        rust_raw.as_bytes(),
        "a private self-join",
    );

    // **Open the teams up** and the same request succeeds — `join_public_teams` is in the default
    // `system_user` role, and the branch is keyed on `AllowOpenInvite` alone.
    // `PUT /teams/{id}/privacy` is not served here, so both changes go through **Go**: it is the
    // server with a team cache to keep warm, and this one reads the row fresh every time.
    for team in [&fixture.go_team, &fixture.rust_team] {
        let (status, body) = call(
            &http,
            GO,
            reqwest::Method::PUT,
            &format!("/api/v4/teams/{team}/privacy"),
            &token,
            Some(&serde_json::json!({"privacy": "O"})),
        )
        .await;
        assert_eq!(status, 200, "the team could not be opened: {body}");
    }
    let (go_status, go_raw) = call(
        &http,
        GO,
        reqwest::Method::POST,
        &format!("/api/v4/teams/{}/members", fixture.go_team),
        &plain,
        Some(&self_body_go),
    )
    .await;
    let (rust_status, rust_raw) = call(
        &http,
        RUST,
        reqwest::Method::POST,
        &format!("/api/v4/teams/{}/members", fixture.rust_team),
        &plain,
        Some(&self_body_rust),
    )
    .await;
    assert_eq!(go_status, 201, "Go allows an open self-join: {go_raw}");
    assert_eq!(
        rust_status, go_status,
        "the open self-join differs: {rust_raw}"
    );
    assert_eq!(
        normalise(&serde_json::from_str(go_raw.trim()).expect("a member")),
        normalise(&serde_json::from_str(rust_raw.trim()).expect("a member")),
        "the self-joined member differs\n go: {go_raw}\nrust: {rust_raw}"
    );

    // **Adding somebody else** needs `add_user_to_team` on the team — which the stock
    // `team_user` role *does* hold on this deployment, so the plain member succeeds. Measured:
    // this test asserted a 403 first and Go answered 201.
    //
    // What the plain member does **not** hold is `manage_team_roles`, so the answer is run
    // through `SanitizeRoleData` — and that is the branch worth pinning, because it puts
    // `delete_at: -1` on the wire for a membership that was just created with `delete_at = 0`.
    let other = create_plain_user(&http, &token, &fixture.go_team, "tmwaddperm2").await;
    let mut sanitised = Vec::new();
    for (base, team) in [(GO, &fixture.go_team), (RUST, &fixture.rust_team)] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/teams/{team}/members"),
            &plain,
            Some(&serde_json::json!({"team_id": team, "user_id": other.id})),
        )
        .await;
        sanitised.push((status, body));
        let _ = base;
    }
    let (go_status, go_raw) = &sanitised[0];
    let (rust_status, rust_raw) = &sanitised[1];
    assert_eq!(
        rust_status, go_status,
        "adding a third party as a plain member differs
 go: {go_raw}
rust: {rust_raw}"
    );
    assert_eq!(*go_status, 201, "Go allows it: {go_raw}");
    assert_eq!(
        normalise(&serde_json::from_str(go_raw.trim()).expect("a member")),
        normalise(&serde_json::from_str(rust_raw.trim()).expect("a member")),
        "the sanitised member differs
 go: {go_raw}
rust: {rust_raw}"
    );
    let member: serde_json::Value = serde_json::from_str(rust_raw.trim()).expect("a member");
    assert_eq!(
        member["delete_at"], -1,
        "SanitizeRoleData's sentinel is -1, not 0: {rust_raw}"
    );
    assert_eq!(member["roles"], "");
    assert_eq!(member["scheme_user"], false);

    // The batch route sanitises the same way, and its permission check sits **after** the team
    // read rather than before it.
    let third = create_plain_user(&http, &token, &fixture.go_team, "tmwaddperm3").await;
    let mut batched = Vec::new();
    for (base, team) in [(GO, &fixture.go_team), (RUST, &fixture.rust_team)] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/teams/{team}/members/batch"),
            &plain,
            Some(&serde_json::json!([{"team_id": team, "user_id": third.id}])),
        )
        .await;
        batched.push((status, body));
        let _ = base;
    }
    assert_eq!(
        batched[1].0, batched[0].0,
        "the batch add differs
 go: {}
rust: {}",
        batched[0].1, batched[1].1
    );
    assert_eq!(
        normalise(&serde_json::from_str(batched[0].1.trim()).expect("a list")),
        normalise(&serde_json::from_str(batched[1].1.trim()).expect("a list")),
        "the batch body differs
 go: {}
rust: {}",
        batched[0].1,
        batched[1].1
    );
    let list: serde_json::Value = serde_json::from_str(batched[1].1.trim()).expect("a list");
    assert_eq!(
        list[0]["delete_at"], -1,
        "the batch sanitises each member too: {}",
        batched[1].1
    );

    common::delete_plain_user(&http, &token, &third.id).await;
    common::delete_plain_user(&http, &token, &other.id).await;
    add_cleanup(&http, &token, &fixture).await;
}

#[tokio::test]
async fn the_add_publishes_added_to_team_twice_and_user_added_per_channel() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let fixture = add_pair(&http, &token, "tmwaddevt").await;
    let user = &fixture.user;

    // Two probes, because the two events are addressed differently and only one of them is
    // deterministic on the joining user's own socket.
    //
    // `added_to_team` is user-addressed: it reaches that user's connections whatever the hub
    // thinks about channels. `user_added` is **channel**-addressed, and whether the brand-new
    // member's own socket is counted as being in the channel yet depends on when each hub last
    // refreshed its membership view — so it is probed on the **admin's** socket instead, which
    // has been in `town-square` and `off-topic` since it created the teams.
    let mut go_socket = SocketProbe::connect(GO, &fixture.user_token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &fixture.user_token).await;
    let mut go_admin = SocketProbe::connect(GO, &token).await;
    let mut rust_admin = SocketProbe::connect(RUST, &token).await;

    for (base, team) in [(GO, &fixture.go_team), (RUST, &fixture.rust_team)] {
        let (status, body) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/teams/{team}/members"),
            &token,
            Some(&serde_json::json!({"team_id": team, "user_id": user})),
        )
        .await;
        assert_eq!(status, 201, "{base} added the member: {body}");
    }
    go_socket.collect_for(Duration::from_millis(1200)).await;
    rust_socket.collect_for(Duration::from_millis(1200)).await;
    go_admin.collect_for(Duration::from_millis(300)).await;
    rust_admin.collect_for(Duration::from_millis(300)).await;

    let go_added = events_for_team(&go_socket, "added_to_team", &fixture.go_team);
    let rust_added = events_for_team(&rust_socket, "added_to_team", &fixture.rust_team);
    // **Two**: `JoinUserToTeam` publishes one and `AddTeamMember` publishes another on top.
    assert_eq!(
        go_added.len(),
        2,
        "Go publishes added_to_team twice for one add: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_added.len(),
        go_added.len(),
        "the added_to_team count differs: {:?}",
        rust_socket.raw
    );
    assert_eq!(rust_added[0]["broadcast"]["user_id"], user.as_str());
    assert_eq!(rust_added[0]["broadcast"]["team_id"], "");
    assert_eq!(rust_added[0]["data"]["user_id"], user.as_str());
    assert_eq!(
        rust_added[0]["data"]["team_id"],
        fixture.rust_team.as_str(),
        "the payload names the team even though the broadcast does not"
    );
    assert_eq!(
        go_added[0]["broadcast"], rust_added[0]["broadcast"],
        "the added_to_team broadcast differs"
    );

    // `JoinDefaultChannels` publishes one channel-addressed `user_added` per default channel —
    // **without** `omit_users`, so the joining user's own socket sees it. That is the opposite of
    // `AddChannelMember`, which omits them.
    let go_user_added = events_for_team(&go_admin, "user_added", &fixture.go_team);
    let rust_user_added = events_for_team(&rust_admin, "user_added", &fixture.rust_team);
    assert_eq!(
        go_user_added.len(),
        2,
        "one per default channel: {:?}",
        go_admin.raw
    );
    assert_eq!(
        rust_user_added.len(),
        go_user_added.len(),
        "the user_added count differs: {:?}",
        rust_admin.raw
    );
    assert_eq!(rust_user_added[0]["data"]["user_id"], user.as_str());
    assert_eq!(
        rust_user_added[0]["data"]["team_id"],
        fixture.rust_team.as_str()
    );
    assert!(
        rust_user_added[0]["broadcast"]["omit_users"].is_null(),
        "the team-join user_added omits nobody: {}",
        rust_user_added[0]
    );
    assert_eq!(
        rust_user_added[0]["broadcast"]["user_id"], "",
        "it is addressed to the channel, not the user"
    );
    assert!(
        !rust_user_added[0]["broadcast"]["channel_id"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "…and the channel id is on the broadcast: {}",
        rust_user_added[0]
    );

    // A **re-add publishes nothing from the join** — `alreadyAdded` short-circuits — but
    // `AddTeamMember`'s own publish still fires, so exactly one more event arrives.
    for (base, team) in [(GO, &fixture.go_team), (RUST, &fixture.rust_team)] {
        let (status, _) = call(
            &http,
            base,
            reqwest::Method::POST,
            &format!("/api/v4/teams/{team}/members"),
            &token,
            Some(&serde_json::json!({"team_id": team, "user_id": user})),
        )
        .await;
        assert_eq!(status, 201, "{base} re-added the member");
    }
    go_socket.collect_for(Duration::from_millis(1200)).await;
    rust_socket.collect_for(Duration::from_millis(1200)).await;

    assert_eq!(
        events_for_team(&go_socket, "added_to_team", &fixture.go_team).len(),
        3,
        "the re-add published exactly one more: {:?}",
        go_socket.raw
    );
    assert_eq!(
        events_for_team(&rust_socket, "added_to_team", &fixture.rust_team).len(),
        events_for_team(&go_socket, "added_to_team", &fixture.go_team).len(),
        "the re-add event count differs: {:?}",
        rust_socket.raw
    );
    go_admin.collect_for(Duration::from_millis(300)).await;
    rust_admin.collect_for(Duration::from_millis(300)).await;
    assert_eq!(
        events_for_team(&go_admin, "user_added", &fixture.go_team).len(),
        2,
        "Go published no further user_added for a re-add: {:?}",
        go_admin.raw
    );
    assert_eq!(
        events_for_team(&rust_admin, "user_added", &fixture.rust_team).len(),
        events_for_team(&go_admin, "user_added", &fixture.go_team).len(),
        "a re-add must not re-join the default channels: {:?}",
        rust_admin.raw
    );

    add_cleanup(&http, &token, &fixture).await;
}
