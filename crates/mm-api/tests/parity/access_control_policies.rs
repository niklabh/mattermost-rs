//! Cross-server parity for the sixteen access-control-policy routes of `api4/access_control.go`
//! and their fourteen local-socket twins, on a build whose access-control service is nil.
//!
//! ```sh
//! scripts/parity.sh --test parity access_control_policies
//! ```
//!
//! Every route is a chain of gates in front of a service that is not there, so what is compared
//! is the *order* of those gates: which 400, 401, 403, 404, 500 or 501 a given caller with a
//! given body reaches first, on both servers. Four callers are used throughout — the system
//! admin, a plain team member, a **team admin** (`manage_team_access_rules`) and a **channel
//! admin** (`manage_channel_access_rules` on one private channel) — because the delegated rungs
//! are what the handlers are made of.
//!
//! # The store decides a team admin's answer, so the suite plants rows
//!
//! `ValidateTeamAdminPolicyOwnership` searches `AccessControlPolicies`, which both servers share
//! and which nothing on this build can write through the API. Three parent policies are planted
//! by SQL: one scoped to the fixture team, one whose only child channel is in it (ownership by
//! inference), and one that is neither. A team admin gets the service's 501 on the first two
//! and the 403 on the third. The child row is also what `ReconcilePolicyTeamScope` and the
//! parent-filtered channel search read.
//!
//! # Holding `PROPERTY_ROWS`
//!
//! The autocomplete route reads the `access_control` property group, whose rows the CPA suites
//! plant and count; the tests that read it hold the same lock those suites do, and the one that
//! plants a field of its own holds it for the plant and the clean-up.

use crate::common;
use crate::common::local_socket::{go_socket, rust_socket, sockets_enabled};
use common::{
    ACTIVE_LICENCE_ROW, GO, PROPERTY_ROWS, RUST, add_user_to_channel,
    assert_error_bodies_match_except_known_gaps, client, create_channel_typed, create_plain_user,
    create_team, go_minted_token, stack_enabled,
};

const AC: &str = "/api/v4/access_control_policies";

/// A parent policy scoped to the fixture team (`scope=team`, `scope_id=<team>`).
const SCOPED: &str = "mmrsabacscoped111111111111";
/// A parent policy with no scope whose one child channel is in the fixture team.
const INFERRED: &str = "mmrsabacinferred1111111111";
/// A parent policy nobody owns.
const NOBODY: &str = "mmrsabacnobody111111111111";
/// A parent's `Data` with the one rule a v0.3 parent must carry to pass `IsValid` — a parent
/// without rules is refused by the store, and Go's reconcile then only logs.
const RULED: &str = r#"{"imports":[],"rules":[{"actions":["membership"],"expression":"true"}]}"#;
/// An id that is valid and matches nothing.
const MISSING: &str = "abcdefghijklmnopqrstuvwxyz";
const ZEROS: &str = "00000000000000000000000000";

struct Fixture {
    team_id: String,
    team2_id: String,
    channel_id: String,
    /// An **archived** private channel in the team, carrying the one child policy. Archived so
    /// that no other suite's server-wide `access_control_policy_enforced` list sees it (they
    /// exclude deleted channels), while the ownership inference join and `GetMany` — neither
    /// filters `DeleteAt` — still do. Planted after the archive: archiving deletes the
    /// channel's policy row.
    child_channel_id: String,
    member: common::PlainUser,
    team_admin: common::PlainUser,
    channel_admin: common::PlainUser,
    stranger: common::PlainUser,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn scheme_admin(client: &reqwest::Client, token: &str, path: &str) {
    let response = client
        .put(format!("{GO}{path}/schemeRoles"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "scheme_user": true, "scheme_admin": true }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success(), "promoting through {path}");
}

/// The three parents and the child, written straight to the shared table.
async fn plant_policies(team_id: &str, channel_id: &str) {
    let pool = common::fixture_pool()
        .await
        .expect("DATABASE_URL names the stack database");
    for (id, name, data) in [
        (
            SCOPED,
            "mmrsabac scoped",
            format!(
                r#"{{"imports":[],"rules":[{{"actions":["membership"],"expression":"true"}}],"scope":"team","scope_id":"{team_id}"}}"#
            ),
        ),
        (
            INFERRED,
            "mmrsabac inferred",
            r#"{"imports":[],"rules":[]}"#.to_owned(),
        ),
        (
            NOBODY,
            "mmrsabac nobody",
            r#"{"imports":[],"rules":[]}"#.to_owned(),
        ),
    ] {
        sqlx::query(
            "INSERT INTO accesscontrolpolicies
                (id, name, type, active, createat, revision, version, data, props)
             VALUES ($1, $2, 'parent', true, 1788600000000, 1, 'v0.3', $3::jsonb, 'null'::jsonb)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(id)
        .bind(name)
        .bind(data)
        .execute(&pool)
        .await
        .expect("the parent policy is planted");
    }
    sqlx::query(
        "INSERT INTO accesscontrolpolicies
            (id, name, type, active, createat, revision, version, data, props)
         VALUES ($1, 'mmrsabac child', 'channel', true, 1788600000000, 1, 'v0.3',
                 $2::jsonb, 'null'::jsonb)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(channel_id)
    .bind(format!(r#"{{"imports":["{INFERRED}"],"rules":[]}}"#))
    .execute(&pool)
    .await
    .expect("the child policy is planted");
}

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            let team_id = create_team(client, token, "abac").await;
            let team2_id = create_team(client, token, "abac2").await;
            let channel_id = create_channel_typed(client, token, &team_id, "abac", "P").await;
            let child_channel_id =
                create_channel_typed(client, token, &team_id, "abacchild", "P").await;
            common::delete_channel(client, token, &child_channel_id).await;
            let member = create_plain_user(client, token, &team_id, "abacm").await;
            let team_admin = create_plain_user(client, token, &team_id, "abact").await;
            let channel_admin = create_plain_user(client, token, &team_id, "abacc").await;
            let stranger = create_plain_user(client, token, &team2_id, "abacs").await;
            add_user_to_channel(client, token, &channel_id, &member.id).await;
            add_user_to_channel(client, token, &channel_id, &channel_admin.id).await;
            scheme_admin(
                client,
                token,
                &format!("/api/v4/teams/{team_id}/members/{}", team_admin.id),
            )
            .await;
            scheme_admin(
                client,
                token,
                &format!("/api/v4/channels/{channel_id}/members/{}", channel_admin.id),
            )
            .await;
            plant_policies(&team_id, &child_channel_id).await;
            Fixture {
                team_id,
                team2_id,
                channel_id,
                child_channel_id,
                member,
                team_admin,
                channel_admin,
                stranger,
            }
        })
        .await
}

#[derive(Clone, Copy, Debug)]
enum Who {
    Admin,
    Member,
    TeamAdmin,
    ChannelAdmin,
    Stranger,
}

impl Who {
    fn token<'a>(self, admin: &'a str, f: &'a Fixture) -> &'a str {
        match self {
            Who::Admin => admin,
            Who::Member => &f.member.token,
            Who::TeamAdmin => &f.team_admin.token,
            Who::ChannelAdmin => &f.channel_admin.token,
            Who::Stranger => &f.stranger.token,
        }
    }
}

async fn send(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, bool, Vec<u8>) {
    let mut request = client
        .request(method.parse().expect("a method"), format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request
            .header("Content-Type", "application/json")
            .body(body.to_owned());
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        served,
        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .unwrap_or_default(),
    )
}

/// Both servers, the expected status, ours served; success bodies byte for byte, error bodies
/// around the known gaps. Returns Go's body.
async fn both_on(
    client: &reqwest::Client,
    servers: (&str, &str),
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    status: u16,
) -> Vec<u8> {
    let (go_status, _, go) = send(client, servers.0, token, method, path, body).await;
    let (rs_status, served, rs) = send(client, servers.1, token, method, path, body).await;
    let context = format!("{method} {path} body={body:?}");
    assert_eq!(
        go_status,
        status,
        "Go {context}: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        status,
        "Rust {context}: {}",
        String::from_utf8_lossy(&rs)
    );
    assert!(served, "{context}: served here");
    if status < 400 {
        assert_eq!(
            go,
            rs,
            "{context}: go={} rust={}",
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs)
        );
    } else {
        assert_error_bodies_match_except_known_gaps(&go, &rs, &context);
    }
    go
}

async fn both(
    client: &reqwest::Client,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
    status: u16,
) -> Vec<u8> {
    both_on(client, (GO, RUST), token, method, path, body, status).await
}

/// Go's error id from a body.
fn error_id(body: &[u8]) -> String {
    let parsed: serde_json::Value = serde_json::from_slice(body).expect("an error body");
    parsed["id"].as_str().unwrap_or_default().to_owned()
}

/// One row of a [`table`]: `(who, method, path, body, status, id)`.
type Row<'a> = (Who, &'a str, &'a str, Option<&'a str>, u16, &'a str);

/// A table of rows, each on both servers.
async fn table(client: &reqwest::Client, admin: &str, f: &Fixture, rows: &[Row<'_>]) {
    for (who, method, path, body, status, id) in rows {
        let go = both(client, who.token(admin, f), method, path, *body, *status).await;
        if *status >= 400 {
            assert_eq!(error_id(&go), *id, "{who:?} {method} {path} body={body:?}");
        }
    }
}

const PERM: &str = "api.context.permissions.app_error";
const BAD_BODY: &str = "api.context.invalid_body_param.app_error";
const BAD_URL: &str = "api.context.invalid_url_param.app_error";
const BAD_PARAM: &str = "api.context.invalid_param.app_error";
const CREATE_501: &str = "app.pap.create_access_control_policy.app_error";
const GET_501: &str = "app.pap.get_policy.app_error";
const DELETE_501: &str = "app.pap.delete_access_control_policy.app_error";
const CHECK_501: &str = "app.pap.check_expression.app_error";
const SEARCH_501: &str = "app.pap.search_access_control_policies.app_error";
const ACTIVE_501: &str = "app.pap.update_access_control_policies_active.app_error";
const AST_501: &str = "app.pap.expression_to_visual_ast.app_error";
const SIMULATE_501: &str = "app.pap.simulate.unavailable";
const ASSIGN_501: &str = "app.pap.assign_access_control_policy_to_channels.app_error";
const UNASSIGN_501: &str = "app.pap.unassign_access_control_policy_from_channels.app_error";
const TEAM_FEATURE_501: &str = "api.access_control_policy.team_membership.feature_disabled";
const SELF_INCLUSION_500: &str = "app.team.access_policies.validation_error.app_error";
const LIMIT_400: &str = "api.access_control_policy.get_channels.limit.app_error";
const FIELDS_LIMIT_400: &str = "api.access_control_policy.get_fields.limit.app_error";
const OUT_OF_SCOPE_403: &str = "api.access_control_policy.simulate.users_out_of_scope.app_error";
const CHANNEL_404: &str = "app.channel.get.existing.app_error";
const COOKIE_401: &str = "api.context.session_cookie_not_allowed.app_error";

/// `PUT /access_control_policies`: the per-type rungs.
#[tokio::test]
async fn create_gates_match_by_type_and_caller() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let with_team = format!("{AC}?team_id={}", f.team_id);
    let parent = r#"{"type":"parent","name":"x"}"#;
    let parent_with_id = format!(r#"{{"type":"parent","name":"x","id":"{MISSING}"}}"#);
    let permission = r#"{"type":"permission","name":"x"}"#;
    let channel = format!(r#"{{"type":"channel","id":"{}","name":"x"}}"#, f.channel_id);
    let channel_bad_id = r#"{"type":"channel","id":"abc","name":"x"}"#;
    let channel_missing = format!(r#"{{"type":"channel","id":"{MISSING}","name":"x"}}"#);
    let channel_permission_rule = format!(
        r#"{{"type":"channel","id":"{}","name":"x","rules":[{{"actions":["upload_file_attachment"],"expression":"true"}}]}}"#,
        f.channel_id
    );
    let team = format!(r#"{{"type":"team","id":"{}","name":"x"}}"#, f.team_id);
    let team_with_rule = format!(
        r#"{{"type":"team","id":"{}","name":"x","rules":[{{"actions":["*"],"expression":"user.attributes.x == \"y\""}}]}}"#,
        f.team_id
    );
    let team_empty_rule = format!(
        r#"{{"type":"team","id":"{}","name":"x","rules":[{{"actions":["*"],"expression":""}}]}}"#,
        f.team_id
    );
    let team_bad_id = r#"{"type":"team","id":"abc","name":"x"}"#;

    table(
        &client,
        &admin,
        f,
        &[
            (Who::Admin, "PUT", AC, Some(parent), 501, CREATE_501),
            (
                Who::Admin,
                "PUT",
                &with_team,
                Some(&parent_with_id),
                501,
                CREATE_501,
            ),
            (Who::Admin, "PUT", AC, Some(permission), 501, CREATE_501),
            (Who::Admin, "PUT", AC, Some(channel_bad_id), 501, CREATE_501),
            (
                Who::Admin,
                "PUT",
                AC,
                Some(&channel_permission_rule),
                501,
                CREATE_501,
            ),
            (
                Who::Admin,
                "PUT",
                AC,
                Some(&team_with_rule),
                501,
                CREATE_501,
            ),
            (
                Who::Admin,
                "PUT",
                AC,
                Some(r#"{"type":"bogus"}"#),
                400,
                BAD_BODY,
            ),
            (
                Who::Admin,
                "PUT",
                AC,
                Some(r#"{"type":"parent""#),
                400,
                BAD_BODY,
            ),
            (
                Who::Admin,
                "PUT",
                AC,
                Some(r#"{"rules":"notalist"}"#),
                400,
                BAD_BODY,
            ),
            (Who::Admin, "PUT", AC, Some("null"), 400, BAD_BODY),
            (Who::Member, "PUT", AC, Some(parent), 403, PERM),
            (Who::Member, "PUT", &with_team, Some(parent), 403, PERM),
            (Who::Member, "PUT", AC, Some(permission), 403, PERM),
            (Who::Member, "PUT", AC, Some(&channel), 403, PERM),
            (Who::Member, "PUT", AC, Some(channel_bad_id), 400, BAD_BODY),
            (Who::Member, "PUT", AC, Some(&team), 403, PERM),
            (Who::Member, "PUT", AC, Some(team_bad_id), 400, BAD_BODY),
            (
                Who::Member,
                "PUT",
                AC,
                Some(r#"{"type":"bogus"}"#),
                400,
                BAD_BODY,
            ),
            // A team admin: the parent needs `?team_id`; with an id, ownership.
            (Who::TeamAdmin, "PUT", AC, Some(parent), 403, PERM),
            (
                Who::TeamAdmin,
                "PUT",
                &with_team,
                Some(parent),
                501,
                CREATE_501,
            ),
            (
                Who::TeamAdmin,
                "PUT",
                &with_team,
                Some(&parent_with_id),
                403,
                PERM,
            ),
            (Who::TeamAdmin, "PUT", AC, Some(permission), 403, PERM),
            // A channel policy: the channel permission, then the stored policy — the 501.
            (Who::TeamAdmin, "PUT", AC, Some(&channel), 501, GET_501),
            (Who::TeamAdmin, "PUT", AC, Some(&channel_missing), 403, PERM),
            (
                Who::TeamAdmin,
                "PUT",
                AC,
                Some(&channel_permission_rule),
                501,
                GET_501,
            ),
            // A team policy: the team permission, each rule's self-inclusion, the stored policy.
            (Who::TeamAdmin, "PUT", AC, Some(&team), 501, GET_501),
            (
                Who::TeamAdmin,
                "PUT",
                AC,
                Some(&team_empty_rule),
                501,
                GET_501,
            ),
            (
                Who::TeamAdmin,
                "PUT",
                AC,
                Some(&team_with_rule),
                500,
                SELF_INCLUSION_500,
            ),
            (Who::TeamAdmin, "PUT", AC, Some(team_bad_id), 400, BAD_BODY),
            (Who::ChannelAdmin, "PUT", AC, Some(parent), 403, PERM),
            (
                Who::ChannelAdmin,
                "PUT",
                &with_team,
                Some(parent),
                403,
                PERM,
            ),
            (Who::ChannelAdmin, "PUT", AC, Some(&channel), 501, GET_501),
            (
                Who::ChannelAdmin,
                "PUT",
                AC,
                Some(&channel_missing),
                403,
                PERM,
            ),
            (Who::ChannelAdmin, "PUT", AC, Some(&team), 403, PERM),
            (Who::Stranger, "PUT", AC, Some(&channel), 403, PERM),
        ],
    )
    .await;
}

/// `GET`/`DELETE /{policy_id}` and `GET /{policy_id}/activate`: the read's team rung, the
/// delete's stricter one, the toggle's cookie barrier and its permission-before-`active` order.
#[tokio::test]
async fn reads_deletes_and_the_single_toggle_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let missing = format!("{AC}/{MISSING}");
    let missing_channel = format!("{AC}/{MISSING}?channelId={}", f.channel_id);
    let missing_team = format!("{AC}/{MISSING}?team_id={}", f.team_id);
    let missing_bad_team = format!("{AC}/{MISSING}?team_id=abc");
    let channel_policy = format!("{AC}/{}", f.channel_id);
    let channel_policy_ctx = format!("{AC}/{}?channelId={}", f.channel_id, f.channel_id);
    let team_policy = format!("{AC}/{}?team_id={}", f.team_id, f.team_id);
    let bad = format!("{AC}/abc");
    let toggle = format!("{AC}/{MISSING}/activate?active=true");
    let toggle_false = format!("{AC}/{MISSING}/activate?active=false");
    let toggle_maybe = format!("{AC}/{MISSING}/activate?active=maybe");
    let toggle_none = format!("{AC}/{MISSING}/activate");
    let toggle_bad = format!("{AC}/abc/activate?active=true");

    table(
        &client,
        &admin,
        f,
        &[
            (Who::Admin, "GET", &missing, None, 501, GET_501),
            (Who::Admin, "GET", &missing_channel, None, 501, GET_501),
            (Who::Admin, "GET", &bad, None, 400, BAD_URL),
            (Who::Member, "GET", &missing, None, 403, PERM),
            (Who::Member, "GET", &missing_team, None, 403, PERM),
            (Who::Member, "GET", &channel_policy_ctx, None, 403, PERM),
            (Who::TeamAdmin, "GET", &missing, None, 403, PERM),
            (Who::TeamAdmin, "GET", &missing_team, None, 403, PERM),
            (Who::TeamAdmin, "GET", &missing_bad_team, None, 403, PERM),
            // The channel admin's own channel: the delegated read is the 501, not a 404, so no
            // fallback — the 403.
            (
                Who::ChannelAdmin,
                "GET",
                &channel_policy_ctx,
                None,
                403,
                PERM,
            ),
            (Who::ChannelAdmin, "GET", &channel_policy, None, 403, PERM),
            (Who::Admin, "DELETE", &missing, None, 501, DELETE_501),
            (Who::Admin, "DELETE", &bad, None, 400, BAD_URL),
            (Who::Member, "DELETE", &missing, None, 403, PERM),
            (Who::Member, "DELETE", &missing_team, None, 403, PERM),
            (Who::TeamAdmin, "DELETE", &missing, None, 403, PERM),
            (Who::TeamAdmin, "DELETE", &missing_team, None, 403, PERM),
            (Who::TeamAdmin, "DELETE", &missing_bad_team, None, 403, PERM),
            // The team's own policy id skips ownership.
            (
                Who::TeamAdmin,
                "DELETE",
                &team_policy,
                None,
                501,
                DELETE_501,
            ),
            (
                Who::ChannelAdmin,
                "DELETE",
                &channel_policy,
                None,
                403,
                PERM,
            ),
            (Who::ChannelAdmin, "DELETE", &team_policy, None, 403, PERM),
            (Who::Admin, "GET", &toggle, None, 501, ACTIVE_501),
            (Who::Admin, "GET", &toggle_false, None, 501, ACTIVE_501),
            (Who::Admin, "GET", &toggle_maybe, None, 400, BAD_BODY),
            (Who::Admin, "GET", &toggle_none, None, 400, BAD_BODY),
            (Who::Admin, "GET", &toggle_bad, None, 400, BAD_URL),
            // The permission comes before `active`.
            (Who::Member, "GET", &toggle_maybe, None, 403, PERM),
            (Who::TeamAdmin, "GET", &toggle, None, 403, PERM),
            (Who::ChannelAdmin, "GET", &toggle, None, 403, PERM),
        ],
    )
    .await;

    // A segment outside gorilla's class never matched: Go's own 404, forwarded.
    let odd = format!("{AC}/abc-def");
    let (go_status, _, go) = send(&client, GO, &admin, "GET", &odd, None).await;
    let (rs_status, served, rs) = send(&client, RUST, &admin, "GET", &odd, None).await;
    assert_eq!((go_status, rs_status), (404, 404));
    assert!(!served, "outside the mux class is Go's 404");
    assert_eq!(go, rs);

    // The CSRF barrier: the same session as a cookie is the 401 on both.
    let cookie = |base: &str| {
        client
            .get(format!("{base}{toggle}"))
            .header("Cookie", format!("MMAUTHTOKEN={admin}"))
            .header("X-Requested-With", "XMLHttpRequest")
    };
    let go = cookie(GO).send().await.expect("Go answers");
    let rs = cookie(RUST).send().await.expect("Rust answers");
    assert_eq!((go.status().as_u16(), rs.status().as_u16()), (401, 401));
    assert_eq!(
        rs.headers().get("x-mmrs-served-by").map(|v| v.as_bytes()),
        Some(&b"rust"[..])
    );
    let go = go.bytes().await.expect("body").to_vec();
    let rs = rs.bytes().await.expect("body").to_vec();
    assert_eq!(error_id(&go), COOKIE_401);
    assert_error_bodies_match_except_known_gaps(&go, &rs, "cookie toggle");
}

/// The CEL tooling — `check`, `test`, `validate_requester`, `visual_ast` and the autocomplete's
/// gate — all share one authorisation shape with different decode names and validations.
#[tokio::test]
async fn cel_tooling_gates_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let check = format!("{AC}/cel/check");
    let test = format!("{AC}/cel/test");
    let validate = format!("{AC}/cel/validate_requester");
    let ast = format!("{AC}/cel/visual_ast");
    let fields = format!("{AC}/cel/autocomplete/fields?limit=10");
    let fields_channel = format!("{fields}&channelId={}", f.channel_id);
    let fields_team = format!("{fields}&team_id={}", f.team_id);
    let fields_bad_channel = format!("{fields}&channelId=abc");
    let plain = r#"{"expression":"true"}"#;
    let with_channel = format!(r#"{{"expression":"true","channelId":"{}"}}"#, f.channel_id);
    let bad_channel = r#"{"expression":"true","channelId":"abc"}"#;
    let with_team = format!(r#"{{"expression":"true","teamId":"{}"}}"#, f.team_id);
    let bad_team = r#"{"expression":"true","teamId":"abc"}"#;
    let with_both = format!(
        r#"{{"expression":"true","teamId":"{}","channelId":"{}"}}"#,
        f.team_id, f.channel_id
    );
    let wrong_channel = format!(
        r#"{{"expression":"true","teamId":"{}","channelId":"{MISSING}"}}"#,
        f.team_id
    );
    let bad_team_good_channel = format!(
        r#"{{"expression":"true","teamId":"abc","channelId":"{}"}}"#,
        f.channel_id
    );
    let truncated = r#"{"expression":"#;

    table(
        &client,
        &admin,
        f,
        &[
            (Who::Admin, "POST", &check, Some(plain), 501, CHECK_501),
            (Who::Admin, "POST", &check, Some(bad_channel), 400, BAD_BODY),
            (
                Who::Admin,
                "POST",
                &check,
                Some(&bad_team_good_channel),
                501,
                CHECK_501,
            ),
            (Who::Admin, "POST", &check, Some(truncated), 400, BAD_BODY),
            (Who::Member, "POST", &check, Some(plain), 403, PERM),
            (Who::Member, "POST", &check, Some(&with_channel), 403, PERM),
            (Who::Member, "POST", &check, Some(&with_both), 403, PERM),
            (Who::TeamAdmin, "POST", &check, Some(plain), 403, PERM),
            (
                Who::TeamAdmin,
                "POST",
                &check,
                Some(&with_channel),
                501,
                CHECK_501,
            ),
            (
                Who::TeamAdmin,
                "POST",
                &check,
                Some(&with_team),
                501,
                CHECK_501,
            ),
            (
                Who::TeamAdmin,
                "POST",
                &check,
                Some(&with_both),
                501,
                CHECK_501,
            ),
            // A channel that is not in the team defeats the team rung and then the channel one.
            (
                Who::TeamAdmin,
                "POST",
                &check,
                Some(&wrong_channel),
                403,
                PERM,
            ),
            (
                Who::TeamAdmin,
                "POST",
                &check,
                Some(&bad_team_good_channel),
                501,
                CHECK_501,
            ),
            (Who::ChannelAdmin, "POST", &check, Some(plain), 403, PERM),
            (
                Who::ChannelAdmin,
                "POST",
                &check,
                Some(&with_channel),
                501,
                CHECK_501,
            ),
            (
                Who::ChannelAdmin,
                "POST",
                &check,
                Some(&with_team),
                403,
                PERM,
            ),
            (
                Who::ChannelAdmin,
                "POST",
                &check,
                Some(&with_both),
                501,
                CHECK_501,
            ),
            (
                Who::Stranger,
                "POST",
                &check,
                Some(&with_channel),
                403,
                PERM,
            ),
            // `test` validates `teamId` too.
            (Who::Admin, "POST", &test, Some(plain), 501, CHECK_501),
            (Who::Admin, "POST", &test, Some(bad_team), 400, BAD_BODY),
            (Who::Admin, "POST", &test, Some(bad_channel), 400, BAD_BODY),
            (Who::Member, "POST", &test, Some(&with_channel), 403, PERM),
            (Who::TeamAdmin, "POST", &test, Some(plain), 403, PERM),
            (
                Who::TeamAdmin,
                "POST",
                &test,
                Some(&with_team),
                501,
                CHECK_501,
            ),
            (
                Who::ChannelAdmin,
                "POST",
                &test,
                Some(&with_channel),
                501,
                CHECK_501,
            ),
            (
                Who::ChannelAdmin,
                "POST",
                &test,
                Some(&with_team),
                403,
                PERM,
            ),
            (Who::Admin, "POST", &validate, Some(plain), 501, CHECK_501),
            (Who::Admin, "POST", &validate, Some(bad_team), 400, BAD_BODY),
            (
                Who::Admin,
                "POST",
                &validate,
                Some(truncated),
                400,
                BAD_BODY,
            ),
            (Who::Member, "POST", &validate, Some(plain), 403, PERM),
            (
                Who::TeamAdmin,
                "POST",
                &validate,
                Some(&with_team),
                501,
                CHECK_501,
            ),
            (
                Who::ChannelAdmin,
                "POST",
                &validate,
                Some(&with_channel),
                501,
                CHECK_501,
            ),
            (Who::ChannelAdmin, "POST", &validate, Some(plain), 403, PERM),
            // `visual_ast` validates `channelId` only.
            (Who::Admin, "POST", &ast, Some(plain), 501, AST_501),
            (Who::Admin, "POST", &ast, Some(bad_team), 501, AST_501),
            (Who::Admin, "POST", &ast, Some(bad_channel), 400, BAD_BODY),
            (Who::Admin, "POST", &ast, Some(truncated), 400, BAD_BODY),
            (Who::Member, "POST", &ast, Some(&with_channel), 403, PERM),
            (Who::TeamAdmin, "POST", &ast, Some(plain), 403, PERM),
            (Who::TeamAdmin, "POST", &ast, Some(&with_team), 501, AST_501),
            (
                Who::ChannelAdmin,
                "POST",
                &ast,
                Some(&with_channel),
                501,
                AST_501,
            ),
            (Who::ChannelAdmin, "POST", &ast, Some(&with_team), 403, PERM),
            // The autocomplete's gate, without touching the field table.
            (Who::Member, "GET", &fields, None, 403, PERM),
            (Who::Member, "GET", &fields_channel, None, 403, PERM),
            (Who::Member, "GET", &fields_bad_channel, None, 400, BAD_BODY),
            (Who::TeamAdmin, "GET", &fields, None, 403, PERM),
            (Who::ChannelAdmin, "GET", &fields_team, None, 403, PERM),
        ],
    )
    .await;
}

/// `POST /cel/simulate_users`: the body validations, the gate, the cross-team resolution and
/// the delegated caller's users-in-scope check.
#[tokio::test]
async fn simulation_is_validated_gated_and_scoped() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let path = format!("{AC}/cel/simulate_users");
    let body = |extra: &str, user: &str| {
        format!(r#"{{"policy":{{"type":"parent"}},"users":[{{"user_id":"{user}"}}]{extra}}}"#)
    };
    let plain = body("", &f.member.id);
    let team = body(&format!(r#","team_id":"{}""#, f.team_id), &f.member.id);
    let channel = body(
        &format!(r#","channel_id":"{}""#, f.channel_id),
        &f.member.id,
    );
    let cross_team = body(
        &format!(
            r#","channel_id":"{}","team_id":"{}""#,
            f.channel_id, f.team2_id
        ),
        &f.member.id,
    );
    let missing_channel = body(
        &format!(r#","channel_id":"{MISSING}","team_id":"{}""#, f.team_id),
        &f.member.id,
    );
    let stranger_in_team = body(&format!(r#","team_id":"{}""#, f.team_id), &f.stranger.id);
    let stranger_in_channel = body(
        &format!(r#","channel_id":"{}""#, f.channel_id),
        &f.stranger.id,
    );
    let bad_user_in_team = body(&format!(r#","team_id":"{}""#, f.team_id), "abc");
    let bad_scope = body(r#","evaluation_scope":"bogus""#, &f.member.id);
    let all_scope = body(r#","evaluation_scope":"all""#, &f.member.id);
    let bad_channel = body(r#","channel_id":"abc""#, &f.member.id);
    let bad_team = body(r#","team_id":"abc""#, &f.member.id);

    table(
        &client,
        &admin,
        f,
        &[
            (Who::Admin, "POST", &path, Some(&plain), 501, SIMULATE_501),
            (Who::Admin, "POST", &path, Some(&team), 501, SIMULATE_501),
            (
                Who::Admin,
                "POST",
                &path,
                Some(&all_scope),
                501,
                SIMULATE_501,
            ),
            (Who::Admin, "POST", &path, Some(&cross_team), 400, BAD_BODY),
            (
                Who::Admin,
                "POST",
                &path,
                Some(&missing_channel),
                404,
                CHANNEL_404,
            ),
            (
                Who::Admin,
                "POST",
                &path,
                Some(&stranger_in_team),
                501,
                SIMULATE_501,
            ),
            (Who::Admin, "POST", &path, Some(&bad_scope), 400, BAD_BODY),
            (Who::Admin, "POST", &path, Some(&bad_channel), 400, BAD_BODY),
            (Who::Admin, "POST", &path, Some(&bad_team), 400, BAD_BODY),
            (
                Who::Admin,
                "POST",
                &path,
                Some(r#"{"policy":{"type":"parent"},"users":[]}"#),
                400,
                BAD_BODY,
            ),
            (
                Who::Admin,
                "POST",
                &path,
                Some(r#"{"users":[{"user_id":"x"}]}"#),
                400,
                BAD_BODY,
            ),
            (
                Who::Admin,
                "POST",
                &path,
                Some(r#"{"policy":"#),
                400,
                BAD_BODY,
            ),
            (Who::Member, "POST", &path, Some(&plain), 403, PERM),
            (Who::Member, "POST", &path, Some(&channel), 403, PERM),
            (Who::Member, "POST", &path, Some(&bad_scope), 400, BAD_BODY),
            (Who::TeamAdmin, "POST", &path, Some(&plain), 403, PERM),
            (
                Who::TeamAdmin,
                "POST",
                &path,
                Some(&team),
                501,
                SIMULATE_501,
            ),
            (
                Who::TeamAdmin,
                "POST",
                &path,
                Some(&channel),
                501,
                SIMULATE_501,
            ),
            (
                Who::TeamAdmin,
                "POST",
                &path,
                Some(&cross_team),
                400,
                BAD_BODY,
            ),
            (
                Who::TeamAdmin,
                "POST",
                &path,
                Some(&missing_channel),
                403,
                PERM,
            ),
            // The users must be in the team (or the channel) for a delegated caller.
            (
                Who::TeamAdmin,
                "POST",
                &path,
                Some(&stranger_in_team),
                403,
                OUT_OF_SCOPE_403,
            ),
            (
                Who::TeamAdmin,
                "POST",
                &path,
                Some(&bad_user_in_team),
                400,
                BAD_PARAM,
            ),
            (Who::ChannelAdmin, "POST", &path, Some(&team), 403, PERM),
            (
                Who::ChannelAdmin,
                "POST",
                &path,
                Some(&channel),
                501,
                SIMULATE_501,
            ),
            (
                Who::ChannelAdmin,
                "POST",
                &path,
                Some(&stranger_in_channel),
                403,
                OUT_OF_SCOPE_403,
            ),
        ],
    )
    .await;
}

/// `POST /search` and `PUT /activate`.
#[tokio::test]
async fn search_and_batch_activation_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let search = format!("{AC}/search");
    let activate = format!("{AC}/activate");
    let team_search = format!(r#"{{"team_id":"{}"}}"#, f.team_id);
    let team2_search = format!(r#"{{"team_id":"{}"}}"#, f.team2_id);
    let entry = format!(r#"{{"entries":[{{"id":"{MISSING}","active":true}}]}}"#);
    let entry_with_team = format!(
        r#"{{"entries":[{{"id":"{MISSING}","active":true}}],"team_id":"{}"}}"#,
        f.team_id
    );
    let team_entry = format!(
        r#"{{"entries":[{{"id":"{}","active":true}}],"team_id":"{}"}}"#,
        f.team_id, f.team_id
    );
    let channel_entry = format!(
        r#"{{"entries":[{{"id":"{}","active":false}}]}}"#,
        f.channel_id
    );

    table(
        &client,
        &admin,
        f,
        &[
            (Who::Admin, "POST", &search, Some("{}"), 501, SEARCH_501),
            (
                Who::Admin,
                "POST",
                &search,
                Some(r#"{"type":"permission"}"#),
                501,
                SEARCH_501,
            ),
            (
                Who::Admin,
                "POST",
                &search,
                Some(&team_search),
                501,
                SEARCH_501,
            ),
            (
                Who::Admin,
                "POST",
                &search,
                Some(r#"{"team_id":"abc"}"#),
                400,
                BAD_BODY,
            ),
            (Who::Admin, "POST", &search, Some("null"), 400, BAD_BODY),
            (Who::Admin, "POST", &search, Some("{"), 400, BAD_BODY),
            (Who::Member, "POST", &search, Some("{}"), 403, PERM),
            (Who::Member, "POST", &search, Some(&team_search), 403, PERM),
            (Who::Member, "POST", &search, Some("null"), 400, BAD_BODY),
            (Who::TeamAdmin, "POST", &search, Some("{}"), 403, PERM),
            (
                Who::TeamAdmin,
                "POST",
                &search,
                Some(&team_search),
                501,
                SEARCH_501,
            ),
            (
                Who::TeamAdmin,
                "POST",
                &search,
                Some(&team2_search),
                403,
                PERM,
            ),
            (
                Who::ChannelAdmin,
                "POST",
                &search,
                Some(&team_search),
                403,
                PERM,
            ),
            (Who::Admin, "PUT", &activate, Some(&entry), 501, ACTIVE_501),
            (
                Who::Admin,
                "PUT",
                &activate,
                Some(&channel_entry),
                501,
                ACTIVE_501,
            ),
            (
                Who::Admin,
                "PUT",
                &activate,
                Some(r#"{"entries":"#),
                400,
                BAD_BODY,
            ),
            (Who::Member, "PUT", &activate, Some(&entry), 403, PERM),
            (
                Who::Member,
                "PUT",
                &activate,
                Some(&entry_with_team),
                403,
                PERM,
            ),
            (
                Who::Member,
                "PUT",
                &activate,
                Some(&channel_entry),
                403,
                PERM,
            ),
            // No entries: no permission loop, the 501 for anyone.
            (
                Who::Member,
                "PUT",
                &activate,
                Some(r#"{"entries":[]}"#),
                501,
                ACTIVE_501,
            ),
            (Who::Member, "PUT", &activate, Some("{}"), 501, ACTIVE_501),
            (Who::Member, "PUT", &activate, Some("null"), 501, ACTIVE_501),
            (Who::TeamAdmin, "PUT", &activate, Some(&entry), 403, PERM),
            (
                Who::TeamAdmin,
                "PUT",
                &activate,
                Some(&entry_with_team),
                403,
                PERM,
            ),
            // The team's own id skips ownership.
            (
                Who::TeamAdmin,
                "PUT",
                &activate,
                Some(&team_entry),
                501,
                ACTIVE_501,
            ),
            (
                Who::TeamAdmin,
                "PUT",
                &activate,
                Some(&channel_entry),
                403,
                PERM,
            ),
            (
                Who::ChannelAdmin,
                "PUT",
                &activate,
                Some(&channel_entry),
                403,
                PERM,
            ),
            (
                Who::ChannelAdmin,
                "PUT",
                &activate,
                Some(&team_entry),
                403,
                PERM,
            ),
        ],
    )
    .await;
}

/// `POST /{id}/assign` and `DELETE /{id}/unassign`: the `team_ids` feature gate first, then
/// the delegated rung, then the two service calls — and a 200 when nothing is named.
#[tokio::test]
async fn assign_and_unassign_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let assign = format!("{AC}/{NOBODY}/assign");
    let unassign = format!("{AC}/{NOBODY}/unassign");
    let assign_bad = format!("{AC}/abc/assign");
    let channels = format!(r#"{{"channel_ids":["{}"]}}"#, f.channel_id);
    let teams = format!(r#"{{"team_ids":["{}"]}}"#, f.team_id);
    let team_only = format!(r#"{{"team_id":"{}"}}"#, f.team_id);
    let team_and_channels = format!(
        r#"{{"team_id":"{}","channel_ids":["{}"]}}"#,
        f.team_id, f.channel_id
    );
    let bad_team_and_channels =
        format!(r#"{{"team_id":"abc","channel_ids":["{}"]}}"#, f.channel_id);
    let team2_and_channels = format!(
        r#"{{"team_id":"{}","channel_ids":["{}"]}}"#,
        f.team2_id, f.channel_id
    );

    table(
        &client,
        &admin,
        f,
        &[
            (Who::Admin, "POST", &assign, Some("{}"), 200, ""),
            (Who::Admin, "POST", &assign, Some(&team_only), 200, ""),
            (Who::Admin, "POST", &assign, Some("null"), 200, ""),
            (
                Who::Admin,
                "POST",
                &assign,
                Some(&channels),
                501,
                ASSIGN_501,
            ),
            (
                Who::Admin,
                "POST",
                &assign,
                Some(&teams),
                501,
                TEAM_FEATURE_501,
            ),
            (
                Who::Admin,
                "POST",
                &assign,
                Some(&team_and_channels),
                501,
                ASSIGN_501,
            ),
            (
                Who::Admin,
                "POST",
                &assign,
                Some(&bad_team_and_channels),
                501,
                ASSIGN_501,
            ),
            (
                Who::Admin,
                "POST",
                &assign,
                Some(r#"{"channel_ids":"#),
                400,
                BAD_BODY,
            ),
            (Who::Admin, "POST", &assign_bad, Some("{}"), 400, BAD_URL),
            (Who::Member, "POST", &assign, Some("{}"), 403, PERM),
            (
                Who::Member,
                "POST",
                &assign,
                Some(&teams),
                501,
                TEAM_FEATURE_501,
            ),
            (
                Who::Member,
                "POST",
                &assign,
                Some(&team_and_channels),
                403,
                PERM,
            ),
            (Who::TeamAdmin, "POST", &assign, Some("{}"), 403, PERM),
            (Who::TeamAdmin, "POST", &assign, Some(&channels), 403, PERM),
            (
                Who::TeamAdmin,
                "POST",
                &assign,
                Some(&teams),
                501,
                TEAM_FEATURE_501,
            ),
            (Who::TeamAdmin, "POST", &assign, Some(&team_only), 403, PERM),
            (
                Who::TeamAdmin,
                "POST",
                &assign,
                Some(&team_and_channels),
                403,
                PERM,
            ),
            (
                Who::TeamAdmin,
                "POST",
                &assign,
                Some(&bad_team_and_channels),
                403,
                PERM,
            ),
            (
                Who::TeamAdmin,
                "POST",
                &assign,
                Some(&team2_and_channels),
                403,
                PERM,
            ),
            (
                Who::ChannelAdmin,
                "POST",
                &assign,
                Some(&team_and_channels),
                403,
                PERM,
            ),
            (Who::Admin, "DELETE", &unassign, Some("{}"), 200, ""),
            (
                Who::Admin,
                "DELETE",
                &unassign,
                Some(&channels),
                501,
                UNASSIGN_501,
            ),
            (
                Who::Admin,
                "DELETE",
                &unassign,
                Some(&teams),
                501,
                TEAM_FEATURE_501,
            ),
            (
                Who::Admin,
                "DELETE",
                &unassign,
                Some(&team_and_channels),
                501,
                UNASSIGN_501,
            ),
            (
                Who::Admin,
                "DELETE",
                &unassign,
                Some(r#"{"channel_ids":"#),
                400,
                BAD_BODY,
            ),
            (Who::Member, "DELETE", &unassign, Some("{}"), 403, PERM),
            (
                Who::Member,
                "DELETE",
                &unassign,
                Some(&teams),
                501,
                TEAM_FEATURE_501,
            ),
            (
                Who::TeamAdmin,
                "DELETE",
                &unassign,
                Some(&team_and_channels),
                403,
                PERM,
            ),
            (
                Who::ChannelAdmin,
                "DELETE",
                &unassign,
                Some(&channels),
                403,
                PERM,
            ),
        ],
    )
    .await;

    // The licensed pair: the stack's licence does not turn `TeamMembershipAccessControlEnabled`
    // on (the ABAC setting is off), so `team_ids` is the same 501 there.
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let pair = common::licensed().await;
    let go = both_on(
        &client,
        (&pair.go, &pair.rust),
        &admin,
        "POST",
        &assign,
        Some(&teams),
        501,
    )
    .await;
    assert_eq!(error_id(&go), TEAM_FEATURE_501);
    both_on(
        &client,
        (&pair.go, &pair.rust),
        &admin,
        "POST",
        &assign,
        Some("{}"),
        200,
    )
    .await;
}

/// `GET /{id}/resources/channels` and `POST /{id}/resources/channels/search` for the callers
/// that do not own anything: the rung, `after`, `limit`, and the real search.
#[tokio::test]
async fn channel_resources_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let list = format!("{AC}/{NOBODY}/resources/channels?limit=10");
    let list_team = format!("{list}&team_id={}", f.team_id);
    let list_bad_team = format!("{list}&team_id=abc");
    let list_bad_limit = format!("{AC}/{NOBODY}/resources/channels?limit=x");
    let list_no_limit = format!("{AC}/{NOBODY}/resources/channels");
    let list_bad_after = format!("{list}&after=abc");
    let list_after = format!("{list}&after={MISSING}");
    let list_bad = format!("{AC}/abc/resources/channels?limit=10");
    let search = format!("{AC}/{NOBODY}/resources/channels/search");
    let search_team = format!("{search}?team_id={}", f.team_id);
    let search_bad = format!("{AC}/abc/resources/channels/search");

    table(
        &client,
        &admin,
        f,
        &[
            (Who::Admin, "GET", &list, None, 501, GET_501),
            (Who::Admin, "GET", &list_team, None, 501, GET_501),
            (Who::Admin, "GET", &list_after, None, 501, GET_501),
            (Who::Admin, "GET", &list_bad_limit, None, 400, LIMIT_400),
            (Who::Admin, "GET", &list_no_limit, None, 400, LIMIT_400),
            (Who::Admin, "GET", &list_bad_after, None, 400, BAD_BODY),
            (Who::Admin, "GET", &list_bad, None, 400, BAD_URL),
            (Who::Member, "GET", &list, None, 403, PERM),
            (Who::Member, "GET", &list_bad_limit, None, 403, PERM),
            (Who::TeamAdmin, "GET", &list, None, 403, PERM),
            (Who::TeamAdmin, "GET", &list_team, None, 403, PERM),
            (Who::TeamAdmin, "GET", &list_bad_team, None, 403, PERM),
            (Who::ChannelAdmin, "GET", &list_team, None, 403, PERM),
            (Who::Admin, "POST", &search, Some(r#"{"term":""}"#), 200, ""),
            (
                Who::Admin,
                "POST",
                &search,
                Some(r#"{"term":"abac"}"#),
                200,
                "",
            ),
            (
                Who::Admin,
                "POST",
                &search_team,
                Some(r#"{"term":""}"#),
                200,
                "",
            ),
            (Who::Admin, "POST", &search, Some("null"), 400, BAD_BODY),
            (Who::Admin, "POST", &search, Some("{"), 400, BAD_BODY),
            (Who::Admin, "POST", &search_bad, Some("{}"), 400, BAD_URL),
            // The rung comes before the body.
            (Who::Member, "POST", &search, Some("{"), 403, PERM),
            (Who::TeamAdmin, "POST", &search_team, Some("{"), 403, PERM),
            (Who::ChannelAdmin, "POST", &search, Some("{}"), 403, PERM),
        ],
    )
    .await;
}

/// The planted rows: a team admin owns the scoped parent and the inferred one, and neither of
/// the two rungs owns the third. Where ownership passes, the answer is the service's; on the
/// channel search it is the **real** list — the inferred parent's one child, byte for byte.
#[tokio::test]
async fn ownership_is_decided_by_the_store() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let team = &f.team_id;
    let team2 = &f.team2_id;

    for (policy, owned) in [(SCOPED, true), (INFERRED, true), (NOBODY, false)] {
        let get = format!("{AC}/{policy}?team_id={team}");
        let get_other = format!("{AC}/{policy}?team_id={team2}");
        let delete = format!("{AC}/{policy}?team_id={team}");
        let activate = format!("{AC}/activate");
        let entry =
            format!(r#"{{"entries":[{{"id":"{policy}","active":true}}],"team_id":"{team}"}}"#);
        let list = format!("{AC}/{policy}/resources/channels?limit=10&team_id={team}");
        let search = format!("{AC}/{policy}/resources/channels/search?team_id={team}");
        let assign = format!("{AC}/{policy}/assign");
        let assign_team = format!(
            r#"{{"team_id":"{team}","channel_ids":["{}"]}}"#,
            f.channel_id
        );
        let (status, id) = if owned { (501, GET_501) } else { (403, PERM) };

        let go = both(&client, &f.team_admin.token, "GET", &get, None, status).await;
        assert_eq!(error_id(&go), id, "{policy}");
        let go = both(&client, &f.team_admin.token, "GET", &get_other, None, 403).await;
        assert_eq!(
            error_id(&go),
            PERM,
            "{policy} is nobody's in the other team"
        );
        let (status, id) = if owned {
            (501, DELETE_501)
        } else {
            (403, PERM)
        };
        let go = both(
            &client,
            &f.team_admin.token,
            "DELETE",
            &delete,
            None,
            status,
        )
        .await;
        assert_eq!(error_id(&go), id, "{policy}");
        let (status, id) = if owned {
            (501, ACTIVE_501)
        } else {
            (403, PERM)
        };
        let go = both(
            &client,
            &f.team_admin.token,
            "PUT",
            &activate,
            Some(&entry),
            status,
        )
        .await;
        assert_eq!(error_id(&go), id, "{policy}");
        let (status, id) = if owned { (501, GET_501) } else { (403, PERM) };
        let go = both(&client, &f.team_admin.token, "GET", &list, None, status).await;
        assert_eq!(error_id(&go), id, "{policy}");
        // Assigning a channel: ownership passes, the channel checks pass, the service refuses.
        let (status, id) = if owned {
            (501, ASSIGN_501)
        } else {
            (403, PERM)
        };
        let go = both(
            &client,
            &f.team_admin.token,
            "POST",
            &assign,
            Some(&assign_team),
            status,
        )
        .await;
        assert_eq!(error_id(&go), id, "{policy}");
        if owned {
            let body = both(
                &client,
                &f.team_admin.token,
                "POST",
                &search,
                Some(r#"{"term":"","include_deleted":true}"#),
                200,
            )
            .await;
            let parsed: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
            let ids: Vec<&str> = parsed["channels"]
                .as_array()
                .expect("a list")
                .iter()
                .filter_map(|c| c["id"].as_str())
                .collect();
            if policy == INFERRED {
                assert_eq!(ids, vec![f.child_channel_id.as_str()], "the one child");
            } else {
                assert!(ids.is_empty(), "{policy} has no child");
            }
        } else {
            both(
                &client,
                &f.team_admin.token,
                "POST",
                &search,
                Some(r#"{"term":""}"#),
                403,
            )
            .await;
        }
    }

    // The system admin's search is the same list without the team filter, and a term that
    // misses is empty.
    let search = format!("{AC}/{INFERRED}/resources/channels/search");
    let body = both(
        &client,
        &admin,
        "POST",
        &search,
        Some(r#"{"term":"","include_deleted":true}"#),
        200,
    )
    .await;
    assert!(
        String::from_utf8_lossy(&body).contains(&f.child_channel_id),
        "the child is listed"
    );
    let body = both(
        &client,
        &admin,
        "POST",
        &search,
        Some(r#"{"term":"zzzz-nothing"}"#),
        200,
    )
    .await;
    assert_eq!(body, br#"{"channels":[],"total_count":0}"#);
}

/// A parent policy's row after an assignment that names nothing.
async fn policy_row(pool: &sqlx::PgPool, id: &str) -> (i32, String, String) {
    sqlx::query_as::<_, (i32, String, String)>(
        "SELECT revision, COALESCE(data->>'scope', ''), COALESCE(data->>'scope_id', '')
           FROM accesscontrolpolicies WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the parent row")
}

async fn reset_inferred(pool: &sqlx::PgPool) {
    sqlx::query("DELETE FROM accesscontrolpolicyhistory WHERE id = $1")
        .bind(INFERRED)
        .execute(pool)
        .await
        .expect("history cleared");
    sqlx::query(
        "UPDATE accesscontrolpolicies
            SET revision = 1, createat = 1788600000000, data = $2::jsonb
          WHERE id = $1",
    )
    .bind(INFERRED)
    .bind(RULED)
    .execute(pool)
    .await
    .expect("the parent reset");
}

/// `ReconcilePolicyTeamScope` is the one write on this build: an empty `assign` (and the
/// pre-flight of an empty `unassign`) stamps the inferred parent's scope from its child, as a
/// new revision with the old one in the history table — on each server in turn, from the same
/// starting row. The scoped parent, with no child, is left alone.
#[tokio::test]
async fn an_empty_assignment_reconciles_the_team_scope() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let pool = common::fixture_pool()
        .await
        .expect("DATABASE_URL names the stack database");
    let assign = format!("{AC}/{INFERRED}/assign");
    let unassign = format!("{AC}/{INFERRED}/unassign");

    for (base, method, path) in [
        (GO, "POST", &assign),
        (RUST, "POST", &assign),
        (GO, "DELETE", &unassign),
        (RUST, "DELETE", &unassign),
    ] {
        reset_inferred(&pool).await;
        assert_eq!(
            policy_row(&pool, INFERRED).await,
            (1, String::new(), String::new())
        );
        let (status, served, body) = send(&client, base, &admin, method, path, Some("{}")).await;
        assert_eq!(
            status,
            200,
            "{base} {method}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(served, base == RUST);
        assert_eq!(body, br#"{"status":"OK"}"#);
        assert_eq!(
            policy_row(&pool, INFERRED).await,
            (2, "team".to_owned(), f.team_id.clone()),
            "{base} {method}: the scope is stamped as revision 2"
        );
        let history: Vec<(i32, String)> = sqlx::query_as(
            "SELECT revision, COALESCE(data->>'scope', '') FROM accesscontrolpolicyhistory WHERE id = $1",
        )
        .bind(INFERRED)
        .fetch_all(&pool)
        .await
        .expect("history rows");
        assert_eq!(
            history,
            vec![(1, String::new())],
            "{base} {method}: the old revision is archived"
        );

        // Running it again changes nothing: the scope already matches.
        let (status, _, _) = send(&client, base, &admin, method, path, Some("{}")).await;
        assert_eq!(status, 200);
        assert_eq!(
            policy_row(&pool, INFERRED).await,
            (2, "team".to_owned(), f.team_id.clone())
        );
    }
    reset_inferred(&pool).await;

    // No children: untouched, on both.
    let scoped_assign = format!("{AC}/{SCOPED}/assign");
    let before = policy_row(&pool, SCOPED).await;
    both(&client, &admin, "POST", &scoped_assign, Some("{}"), 200).await;
    assert_eq!(policy_row(&pool, SCOPED).await, before);
}

/// `GET /cel/autocomplete/fields`: the four native descriptors on the first page, byte for
/// byte, for every caller the gate admits; the `after` and `limit` rules; and, with a field
/// planted in the group, the unlicensed 500 and the licensed page.
#[tokio::test]
async fn autocomplete_fields_match_byte_for_byte() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let fields = format!("{AC}/cel/autocomplete/fields");

    let native_names = |body: &[u8]| -> Vec<String> {
        let parsed: serde_json::Value = serde_json::from_slice(body).expect("a list");
        parsed
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|field| field["id"].as_str().map(ToOwned::to_owned))
            .collect()
    };
    let four = [
        "native_user_attribute_email",
        "native_user_attribute_verified",
        "native_user_attribute_isbot",
        "native_user_attribute_createat",
    ];

    let body = both(
        &client,
        &admin,
        "GET",
        &format!("{fields}?limit=10"),
        None,
        200,
    )
    .await;
    assert_eq!(native_names(&body), four);
    assert!(!body.ends_with(b"\n"), "json.Marshal, no newline");
    let body = both(
        &client,
        &admin,
        "GET",
        &format!("{fields}?limit=1&after={ZEROS}"),
        None,
        200,
    )
    .await;
    assert_eq!(
        native_names(&body),
        four,
        "the sentinel is the first page too"
    );
    let body = both(
        &client,
        &admin,
        "GET",
        &format!("{fields}?limit=100&after={MISSING}"),
        None,
        200,
    )
    .await;
    assert_eq!(body, b"[]", "a later page has no native fields");
    let body = both(
        &client,
        &f.team_admin.token,
        "GET",
        &format!("{fields}?limit=10&team_id={}", f.team_id),
        None,
        200,
    )
    .await;
    assert_eq!(native_names(&body), four);
    let body = both(
        &client,
        &f.channel_admin.token,
        "GET",
        &format!("{fields}?limit=10&channelId={}", f.channel_id),
        None,
        200,
    )
    .await;
    assert_eq!(native_names(&body), four);
    for query in ["", "?limit=0", "?limit=101", "?limit=x", "?limit=+10x"] {
        let go = both(
            &client,
            &admin,
            "GET",
            &format!("{fields}{query}"),
            None,
            400,
        )
        .await;
        assert_eq!(error_id(&go), FIELDS_LIMIT_400, "{query:?}");
    }
    let go = both(
        &client,
        &admin,
        "GET",
        &format!("{fields}?limit=10&after=abc"),
        None,
        400,
    )
    .await;
    assert_eq!(error_id(&go), BAD_BODY);

    // A field in the group: unlicensed, the hook's 403 becomes this route's 500; licensed, the
    // page carries it after the four — and `after` names it without excluding it, since the
    // cursor compares `CreateAt` first.
    let pool = common::fixture_pool()
        .await
        .expect("DATABASE_URL names the stack database");
    let group: String =
        sqlx::query_scalar("SELECT id FROM propertygroups WHERE name = 'access_control'")
            .fetch_one(&pool)
            .await
            .expect("the group");
    const FIELD: &str = "mmrsabacfield1111111111111";
    sqlx::query(
        "INSERT INTO propertyfields
            (id, groupid, name, type, attrs, targetid, targettype, objecttype, protected,
             createat, updateat, deleteat)
         VALUES ($1, $2, 'mmrsabacfield', 'text',
                 '{\"visibility\":\"when_set\",\"sort_order\":1,\"value_type\":\"\"}'::jsonb,
                 '', 'system', 'user', false, 1788600000000, 1788600000000, 0)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(FIELD)
    .bind(&group)
    .execute(&pool)
    .await
    .expect("the field is planted");

    let go = both(
        &client,
        &admin,
        "GET",
        &format!("{fields}?limit=10"),
        None,
        500,
    )
    .await;
    assert_eq!(
        error_id(&go),
        "app.pap.get_access_control_auto_complete.app_error"
    );

    let pair = common::licensed().await;
    let body = both_on(
        &client,
        (&pair.go, &pair.rust),
        &admin,
        "GET",
        &format!("{fields}?limit=10"),
        None,
        200,
    )
    .await;
    let mut five: Vec<&str> = four.to_vec();
    five.push(FIELD);
    assert_eq!(native_names(&body), five);
    let body = both_on(
        &client,
        (&pair.go, &pair.rust),
        &admin,
        "GET",
        &format!("{fields}?limit=10&after={FIELD}"),
        None,
        200,
    )
    .await;
    assert_eq!(native_names(&body), [FIELD]);

    sqlx::query("DELETE FROM propertyfields WHERE id = $1")
        .bind(FIELD)
        .execute(&pool)
        .await
        .expect("the field is removed");
}

// ---------------------------------------------------------------------------------------------
// The local socket
// ---------------------------------------------------------------------------------------------

async fn over(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, bool, Vec<u8>) {
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("Host", "localhost");
    let body = match body {
        Some(body) => {
            request = request
                .header("Content-Type", "application/json")
                .header("Content-Length", body.len().to_string());
            axum::body::Body::from(body.to_owned())
        }
        None => axum::body::Body::empty(),
    };
    let response = mm_api::local::send_over_unix(socket, request.body(body).expect("builds"))
        .await
        .unwrap_or_else(|e| panic!("{method} {path} over {}: {e}", socket.display()));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, served, bytes)
}

/// The fourteen pairs of `InitAccessControlPolicyLocal`, and the two the local mux does not
/// register. The unrestricted session is a system admin everywhere, so each served route is
/// its app function's answer.
#[tokio::test]
async fn the_local_pairs_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let f = fixture(&client, &admin).await;
    let _rows = PROPERTY_ROWS.lock().await;
    let go = go_socket().expect("checked");
    let rs = rust_socket().expect("checked");

    let channel = format!(r#"{{"type":"channel","id":"{}","name":"x"}}"#, f.channel_id);
    let channel_ids = format!(r#"{{"channel_ids":["{}"]}}"#, f.channel_id);
    let team_ids = format!(r#"{{"team_ids":["{}"]}}"#, f.team_id);
    let missing = format!("{AC}/{MISSING}");
    let nobody_assign = format!("{AC}/{NOBODY}/assign");
    let nobody_unassign = format!("{AC}/{NOBODY}/unassign");
    let list = format!("{AC}/{NOBODY}/resources/channels?limit=10");
    let list_bad = format!("{AC}/{NOBODY}/resources/channels?limit=x");
    let search = format!("{AC}/{INFERRED}/resources/channels/search");
    let fields = format!("{AC}/cel/autocomplete/fields?limit=10");
    let fields_bad = format!("{AC}/cel/autocomplete/fields?limit=x");
    let activate = format!("{AC}/activate");
    let entry = format!(r#"{{"entries":[{{"id":"{MISSING}","active":true}}]}}"#);
    let bad = format!("{AC}/abc");

    let rows: [(&str, &str, Option<&str>, u16, &str); 24] = [
        (
            "PUT",
            AC,
            Some(r#"{"type":"parent","name":"x"}"#),
            501,
            CREATE_501,
        ),
        ("PUT", AC, Some(&channel), 501, CREATE_501),
        ("PUT", AC, Some(r#"{"type":"bogus"}"#), 400, BAD_BODY),
        ("POST", &format!("{AC}/search"), Some("{}"), 501, SEARCH_501),
        ("POST", &format!("{AC}/search"), Some("null"), 400, BAD_BODY),
        ("PUT", &activate, Some(&entry), 501, ACTIVE_501),
        (
            "POST",
            &format!("{AC}/cel/check"),
            Some(r#"{"expression":"true"}"#),
            501,
            CHECK_501,
        ),
        (
            "POST",
            &format!("{AC}/cel/test"),
            Some(r#"{"expression":"true"}"#),
            501,
            CHECK_501,
        ),
        (
            "POST",
            &format!("{AC}/cel/validate_requester"),
            Some(r#"{"expression":"true"}"#),
            501,
            CHECK_501,
        ),
        ("GET", &fields, None, 200, ""),
        ("GET", &fields_bad, None, 400, FIELDS_LIMIT_400),
        (
            "POST",
            &format!("{AC}/cel/visual_ast"),
            Some(r#"{"expression":"true"}"#),
            501,
            AST_501,
        ),
        ("GET", &missing, None, 501, GET_501),
        ("GET", &bad, None, 400, BAD_URL),
        ("DELETE", &missing, None, 501, DELETE_501),
        ("POST", &nobody_assign, Some("{}"), 200, ""),
        ("POST", &nobody_assign, Some(&channel_ids), 501, ASSIGN_501),
        (
            "POST",
            &nobody_assign,
            Some(&team_ids),
            501,
            TEAM_FEATURE_501,
        ),
        ("DELETE", &nobody_unassign, Some("{}"), 200, ""),
        (
            "DELETE",
            &nobody_unassign,
            Some(&channel_ids),
            501,
            UNASSIGN_501,
        ),
        ("GET", &list, None, 501, GET_501),
        ("GET", &list_bad, None, 400, LIMIT_400),
        (
            "POST",
            &search,
            Some(r#"{"term":"","include_deleted":true}"#),
            200,
            "",
        ),
        ("POST", &search, Some("null"), 400, BAD_BODY),
    ];
    for (method, path, body, status, id) in rows {
        let (go_status, _, go_body) = over(&go, method, path, body).await;
        let (rs_status, served, rs_body) = over(&rs, method, path, body).await;
        let context = format!("local {method} {path} body={body:?}");
        assert_eq!(
            go_status,
            status,
            "Go {context}: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            status,
            "Rust {context}: {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert!(served, "{context}: served here");
        if status < 400 {
            assert_eq!(go_body, rs_body, "{context}");
        } else {
            assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
            assert_eq!(error_id(&go_body), id, "{context}");
        }
    }

    // Not registered locally: the local mux's 404, which ours forwards over the socket.
    for (method, path, body) in [
        ("GET", format!("{AC}/{MISSING}/activate?active=true"), None),
        (
            "POST",
            format!("{AC}/cel/simulate_users"),
            Some(r#"{"policy":{"type":"parent"},"users":[{"user_id":"x"}]}"#),
        ),
    ] {
        let (go_status, _, go_body) = over(&go, method, &path, body).await;
        let (rs_status, served, rs_body) = over(&rs, method, &path, body).await;
        assert_eq!((go_status, rs_status), (404, 404), "{method} {path}");
        assert!(!served, "{method} {path}: forwarded");
        assert_eq!(go_body, rs_body, "{method} {path}");
    }
}
