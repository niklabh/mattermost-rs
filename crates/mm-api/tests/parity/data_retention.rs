//! Cross-server parity for the fifteen migrated `/data_retention` routes.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity data_retention
//! ```
//!
//! # Every route answers 501; the test is about what comes *before* that
//!
//! `App.DataRetention()` is nil on the Team Edition binary, so the licence refusal is the whole
//! of each route here. What varies — and what this suite pins — is the order of the checks ahead
//! of it, which differs in almost every handler. Three orderings are directly observable:
//!
//! * a **malformed body** is a 400 on the five routes that decode one, even for a caller with no
//!   permission, because the decode comes first;
//! * a **caller without the permission** is a 403 rather than a 501, because the permission check
//!   comes before the app call;
//! * a **malformed policy id** is a **501, not a 400**, because Go throws away the result of
//!   `RequirePolicyId` on eleven of these routes.
//!
//! The last one is the reason this file exists rather than a proxy entry.

use crate::common;

use common::{ACTIVE_LICENCE_ROW, GO, RUST, client, go_minted_token, stack_enabled};

const LICENSE_ERROR: &str = "ent.data_retention.generic.license.error";

/// A well-formed policy id that is not a policy, and a malformed one. Neither exists.
const GOOD_ID: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";
const BAD_ID: &str = "short";

/// A reader holding only `sysconsole_read_compliance_data_retention_policy`, and a writer holding
/// only the `write` half.
static READERS: tokio::sync::OnceCell<Option<(String, String, String)>> =
    tokio::sync::OnceCell::const_new();

/// `(read_only_token, write_only_token, plain_token)`.
async fn readers(client: &reqwest::Client, admin: &str) -> Option<(String, String, String)> {
    READERS
        .get_or_init(|| async {
            common::purge_api_fixtures().await;
            let team = common::create_team(client, admin, "dataret").await;

            let make = async |tag: &str, permission: &str| -> String {
                let role = common::plant_role(tag, permission).await.expect("planted");
                let user = common::create_plain_user(client, admin, &team, tag).await;
                common::set_user_roles(&user.id, &format!("system_user {role}")).await;
                common::login_plain_user(client, tag).await
            };

            let read = make(
                "dataretread",
                "sysconsole_read_compliance_data_retention_policy",
            )
            .await;
            let write = make(
                "dataretwrite",
                "sysconsole_write_compliance_data_retention_policy",
            )
            .await;
            let plain = common::create_plain_user(client, admin, &team, "daretplain")
                .await
                .token;
            Some((read, write, plain))
        })
        .await
        .clone()
}

/// Send one request to both servers and assert they agree on status and body.
async fn both(
    client: &reqwest::Client,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<&[u8]>,
) -> (u16, serde_json::Value) {
    let call = async |base: &str| {
        let mut request = client
            .request(method.clone(), format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json");
        if let Some(body) = body {
            request = request.body(body.to_vec());
        }
        let response = request.send().await.expect("reachable");
        let status = response.status().as_u16();
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (
            status,
            served_by,
            response.bytes().await.expect("reads").to_vec(),
        )
    };

    let (go_status, _, go) = call(GO).await;
    let (rs_status, served_by, rs) = call(RUST).await;
    assert_eq!(
        served_by.as_deref(),
        Some("rust"),
        "{method} {path} is ours to answer"
    );
    assert_eq!(rs_status, go_status, "{method} {path}");
    let parsed =
        common::assert_error_bodies_match_except_known_gaps(&go, &rs, &format!("{method} {path}"));
    (go_status, parsed)
}

/// Every route, as an admin: fifteen 501s with the same error id.
#[tokio::test]
async fn every_route_is_the_same_licence_refusal() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = common::logged_in_user_id();

    let policy_body: &[u8] = br#"{"display_name":"mmrs would-be policy","post_duration":30}"#;
    let ids: &[u8] = br#"["aaaaaaaaaaaaaaaaaaaaaaaaaa"]"#;

    let cases: Vec<(reqwest::Method, String, Option<&[u8]>)> = vec![
        (
            reqwest::Method::GET,
            "/api/v4/data_retention/policy".into(),
            None,
        ),
        (
            reqwest::Method::GET,
            "/api/v4/data_retention/policies".into(),
            None,
        ),
        (
            reqwest::Method::GET,
            "/api/v4/data_retention/policies_count".into(),
            None,
        ),
        (
            reqwest::Method::POST,
            "/api/v4/data_retention/policies".into(),
            Some(policy_body),
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/data_retention/policies/{GOOD_ID}"),
            None,
        ),
        (
            reqwest::Method::PATCH,
            format!("/api/v4/data_retention/policies/{GOOD_ID}"),
            Some(policy_body),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/data_retention/policies/{GOOD_ID}"),
            None,
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/teams"),
            None,
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/teams"),
            Some(ids),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/teams"),
            Some(ids),
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/channels"),
            None,
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/channels"),
            Some(ids),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/channels"),
            Some(ids),
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/users/{me}/data_retention/team_policies"),
            None,
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/users/{me}/data_retention/channel_policies"),
            None,
        ),
    ];
    assert_eq!(cases.len(), 15, "all fifteen migrated routes");

    for (method, path, body) in cases {
        let (status, parsed) = both(&client, &token, method.clone(), &path, body).await;
        assert_eq!(status, 501, "{method} {path}");
        assert_eq!(parsed["id"], LICENSE_ERROR, "{method} {path}");
    }
}

/// **The dead id check.** A malformed policy id is a 501, not a 400, on all eleven routes that
/// take one — because Go calls `RequirePolicyId` and never looks at `c.Err`.
#[tokio::test]
async fn a_malformed_policy_id_is_a_licence_refusal_and_not_a_400() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let ids: &[u8] = br#"["aaaaaaaaaaaaaaaaaaaaaaaaaa"]"#;

    for (method, suffix, body) in [
        (reqwest::Method::GET, "", None),
        (reqwest::Method::DELETE, "", None),
        (reqwest::Method::GET, "/teams", None),
        (reqwest::Method::POST, "/teams", Some(ids)),
        (reqwest::Method::DELETE, "/teams", Some(ids)),
        (reqwest::Method::GET, "/channels", None),
        (reqwest::Method::POST, "/channels", Some(ids)),
        (reqwest::Method::DELETE, "/channels", Some(ids)),
    ] {
        let path = format!("/api/v4/data_retention/policies/{BAD_ID}{suffix}");
        let (status, parsed) = both(&client, &token, method.clone(), &path, body).await;
        assert_eq!(
            status, 501,
            "`RequirePolicyId`'s 400 is overwritten by the app error: {method} {path}"
        );
        assert_eq!(parsed["id"], LICENSE_ERROR);
    }

    // `PATCH` is the exception among the id-taking routes, and only because its **body** is
    // decoded first: a good body still gives 501 on a bad id.
    let path = format!("/api/v4/data_retention/policies/{BAD_ID}");
    let (status, _) = both(
        &client,
        &token,
        reqwest::Method::PATCH,
        &path,
        Some(br#"{"display_name":"x"}"#),
    )
    .await;
    assert_eq!(status, 501, "a good body and a bad id is still the refusal");
}

/// **The body comes first.** A malformed body is a 400 on the five routes that decode one, and the
/// two shapes produce two different error ids.
#[tokio::test]
async fn a_malformed_body_is_a_400_before_the_licence_is_read() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // `RetentionPolicyWithTeamAndChannelIDs` — `api.context.invalid_body_param.app_error`.
    for (method, path) in [
        (
            reqwest::Method::POST,
            "/api/v4/data_retention/policies".to_owned(),
        ),
        (
            reqwest::Method::PATCH,
            format!("/api/v4/data_retention/policies/{GOOD_ID}"),
        ),
    ] {
        let (status, parsed) = both(&client, &token, method.clone(), &path, Some(b"{")).await;
        assert_eq!(status, 400, "{method} {path}");
        assert_eq!(parsed["id"], "api.context.invalid_body_param.app_error");
    }

    // `SortedArrayFromJSON` — `api.payload.parse.error`, a **different** id for the same class of
    // failure, on the four id-list routes.
    for (method, suffix) in [
        (reqwest::Method::POST, "/teams"),
        (reqwest::Method::DELETE, "/teams"),
        (reqwest::Method::POST, "/channels"),
        (reqwest::Method::DELETE, "/channels"),
    ] {
        let path = format!("/api/v4/data_retention/policies/{GOOD_ID}{suffix}");
        let (status, parsed) = both(&client, &token, method.clone(), &path, Some(b"[")).await;
        assert_eq!(status, 400, "{method} {path}");
        assert_eq!(parsed["id"], "api.payload.parse.error", "{method} {path}");
    }

    // `null` is **not** an error: `SortedArrayFromJSON` returns `(nil, nil)` and the handler
    // carries on to the refusal.
    let path = format!("/api/v4/data_retention/policies/{GOOD_ID}/teams");
    let (status, parsed) = both(&client, &token, reqwest::Method::POST, &path, Some(b"null")).await;
    assert_eq!(
        status, 501,
        "a JSON null decodes to a nil slice, not an error"
    );
    assert_eq!(parsed["id"], LICENSE_ERROR);
}

/// **The permission comes before the app call**, so a caller without it gets 403 rather than 501 —
/// and the read and write halves are two different permissions.
#[tokio::test]
async fn each_half_needs_its_own_permission() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let Some((read_token, write_token, plain_token)) = readers(&client, &admin).await else {
        return; // no DATABASE_URL
    };

    let reads: Vec<(reqwest::Method, String, Option<&[u8]>)> = vec![
        (
            reqwest::Method::GET,
            "/api/v4/data_retention/policies".into(),
            None,
        ),
        (
            reqwest::Method::GET,
            "/api/v4/data_retention/policies_count".into(),
            None,
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/data_retention/policies/{GOOD_ID}"),
            None,
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/teams"),
            None,
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/channels"),
            None,
        ),
    ];
    let ids: &[u8] = br#"["aaaaaaaaaaaaaaaaaaaaaaaaaa"]"#;
    let writes: Vec<(reqwest::Method, String, Option<&[u8]>)> = vec![
        (
            reqwest::Method::POST,
            "/api/v4/data_retention/policies".into(),
            Some(br#"{"display_name":"x"}"#),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/data_retention/policies/{GOOD_ID}"),
            None,
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/teams"),
            Some(ids),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/channels"),
            Some(ids),
        ),
    ];

    for (method, path, body) in &reads {
        let (status, _) = both(&client, &read_token, method.clone(), path, *body).await;
        assert_eq!(status, 501, "the read permission admits: {method} {path}");
        let (status, parsed) = both(&client, &write_token, method.clone(), path, *body).await;
        assert_eq!(
            status, 403,
            "the write permission does not: {method} {path}"
        );
        assert_eq!(parsed["id"], "api.context.permissions.app_error");
    }

    for (method, path, body) in &writes {
        let (status, _) = both(&client, &write_token, method.clone(), path, *body).await;
        assert_eq!(status, 501, "the write permission admits: {method} {path}");
        let (status, _) = both(&client, &read_token, method.clone(), path, *body).await;
        assert_eq!(status, 403, "the read permission does not: {method} {path}");
    }

    // **`getGlobalPolicy` refuses nobody.** A plain user with neither permission gets the same
    // 501 an admin does — the one route in the file with no check at all.
    let (status, parsed) = both(
        &client,
        &plain_token,
        reqwest::Method::GET,
        "/api/v4/data_retention/policy",
        None,
    )
    .await;
    assert_eq!(status, 501, "no permission check on the global policy");
    assert_eq!(parsed["id"], LICENSE_ERROR);

    // And a plain user *is* refused everything else, so the line above is about that route.
    let (status, _) = both(
        &client,
        &plain_token,
        reqwest::Method::GET,
        "/api/v4/data_retention/policies",
        None,
    )
    .await;
    assert_eq!(status, 403);
}

/// The two per-user routes: **self needs nothing, someone else needs `manage_system`**, and their
/// id check is the only live one in the file.
#[tokio::test]
async fn the_per_user_routes_are_self_or_manage_system() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let Some((read_token, _, plain_token)) = readers(&client, &admin).await else {
        return;
    };
    let me = common::logged_in_user_id();

    for kind in ["team_policies", "channel_policies"] {
        // A plain user asking about **itself**: no permission at all, straight to the refusal.
        let own = format!("/api/v4/users/me/data_retention/{kind}");
        let (status, parsed) = both(&client, &plain_token, reqwest::Method::GET, &own, None).await;
        assert_eq!(status, 501, "a user may always ask about itself: {own}");
        assert_eq!(parsed["id"], LICENSE_ERROR);

        // The same user asking about **someone else** needs `manage_system`.
        let other = format!("/api/v4/users/{me}/data_retention/{kind}");
        let (status, parsed) =
            both(&client, &plain_token, reqwest::Method::GET, &other, None).await;
        assert_eq!(
            status, 403,
            "someone else's policies need manage_system: {other}"
        );
        assert_eq!(parsed["id"], "api.context.permissions.app_error");

        // **The gate is `manage_system`, not a sysconsole read.** Every other route in this file
        // uses one of the two data-retention sysconsole permissions; these two do not, and a
        // caller holding the read permission is still refused someone else's policies. Without
        // this the swap is invisible — a plain user holds neither permission and is refused
        // either way, and an admin holds both and is admitted either way.
        let (status, parsed) = both(&client, &read_token, reqwest::Method::GET, &other, None).await;
        assert_eq!(
            status, 403,
            "`sysconsole_read_compliance_data_retention_policy` does not open this route: {other}"
        );
        assert_eq!(parsed["id"], "api.context.permissions.app_error");

        // **The live id check.** These two routes *do* test `c.Err` after `RequireUserId`, so a
        // malformed user id is a 400 here where a malformed policy id is a 501 everywhere else.
        let bad = format!("/api/v4/users/{BAD_ID}/data_retention/{kind}");
        let (status, parsed) = both(&client, &admin, reqwest::Method::GET, &bad, None).await;
        assert_eq!(
            status, 400,
            "`RequireUserId`'s result is checked here: {bad}"
        );
        assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");
    }
}

/// The two `/search` children must still be forwarded — they are not licence-gated and this
/// server does not answer them.
#[tokio::test]
async fn the_searches_and_other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let served_by = async |method: reqwest::Method, path: String| -> Option<String> {
        client
            .request(method, format!("{RUST}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({"term": "x"}))
            .send()
            .await
            .expect("we answer")
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    for suffix in ["teams/search", "channels/search"] {
        let path = format!("/api/v4/data_retention/policies/{GOOD_ID}/{suffix}");
        assert_eq!(
            served_by(reqwest::Method::POST, path.clone())
                .await
                .as_deref(),
            Some("go"),
            "{path} is an ordinary search with no licence gate"
        );
    }

    for (method, path) in [
        (
            reqwest::Method::PUT,
            "/api/v4/data_retention/policies".to_owned(),
        ),
        (
            reqwest::Method::POST,
            "/api/v4/data_retention/policy".to_owned(),
        ),
        (
            reqwest::Method::PUT,
            format!("/api/v4/data_retention/policies/{GOOD_ID}"),
        ),
        (
            reqwest::Method::PATCH,
            format!("/api/v4/data_retention/policies/{GOOD_ID}/teams"),
        ),
    ] {
        assert_eq!(
            served_by(method.clone(), path.clone()).await.as_deref(),
            Some("go"),
            "{method} {path} must be forwarded"
        );
    }
}
