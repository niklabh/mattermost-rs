//! Cross-server parity for the routes the 2026-09-13 licence sweep stopped forwarding "on any
//! licence", measured against the **licensed pair** (`common::licensed`): the enterprise-ready Go
//! oracle with the stack's signed Enterprise licence, and an mm-api carrying the same licence.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity licensed_sweep
//! ```
//!
//! Every route here has an unlicensed suite of its own that still compares the stack's ordinary
//! pair; this file is only the half those suites could never reach. Three kinds of route sit
//! here, and the test names say which:
//!
//! - **served both sides of the gate** — the licence body answered the question Go asks
//!   (a tier, a feature flag, `IsCloud`), and what sits behind it was already ported;
//! - **the same refusal licensed or not** — Go's answer past the licence gate is a nil enterprise
//!   interface (`Saml()`, `Ldap()`, `DataRetention()`, the licence manager, the cluster), which no
//!   build from this tree has, so the licensed oracle refuses exactly as the unlicensed one does;
//! - **forwarded on the precise predicate** — what is behind the gate is not ported (the scheme
//!   writes, a priority post's `PostsPriority` row), so the licensed request is handed to Go, but
//!   only once the gate Go itself applies has passed.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, LicensedPair, a_team_and_channel_the_user_is_in,
    assert_error_bodies_match_except_known_gaps, client, create_channel, create_plain_user,
    create_team, delete_channel, delete_plain_user, fetch_licensed_pair, go_minted_token, licensed,
    stack_enabled,
};

/// `(status, body, served_by_rust)` from one server, any method.
async fn send(
    http: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    token: Option<&str>,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> (u16, Vec<u8>, bool) {
    let mut request = http
        .request(method, format!("{base}{path}"))
        .header("Content-Type", content_type)
        .body(body.to_vec());
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let by_rust = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        response.bytes().await.expect("a body").to_vec(),
        by_rust,
    )
}

/// Both licensed servers, asserting the Rust one served it itself.
async fn send_pair(
    http: &reqwest::Client,
    pair: &LicensedPair,
    method: reqwest::Method,
    token: Option<&str>,
    path: &str,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let (go_status, go, _) = send(
        http,
        &pair.go,
        method.clone(),
        token,
        path,
        "application/json",
        body,
    )
    .await;
    let (rs_status, rs, by_rust) = send(
        http,
        &pair.rust,
        method,
        token,
        path,
        "application/json",
        body,
    )
    .await;
    assert!(
        by_rust,
        "{path} was forwarded by the licensed mm-api, so this proves nothing"
    );
    ((go_status, go), (rs_status, rs))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ---------------------------------------------------------------------------------------------
// The same refusal licensed or not: nil enterprise interfaces
// ---------------------------------------------------------------------------------------------

/// `GetClusterStatus`: `a.Cluster()` is nil on every build from this tree, so the licensed
/// oracle answers `[]` — three bytes, no newline — and so do we, without a forward.
#[tokio::test]
async fn cluster_status_is_the_same_empty_list_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, Some(&admin), "/api/v4/cluster/status").await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200);
    assert_eq!(go, b"[]", "no cluster interface, no roster, no newline");
    assert_eq!(rs, go);
}

/// The licence manager, the SAML module and the LDAP module are all nil here, licensed or not:
/// each route's refusal on the licensed pair is the one its unlicensed suite already pins.
/// `getLdapGroups` is the interesting one — its licence gate passes (the oracle has
/// `LDAPGroups`), and the refusal past it is a *different* 501, `ent.ldap.app_error`.
#[tokio::test]
async fn nil_interface_refusals_are_the_same_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;

    for (path, token, status, id) in [
        (
            "/api/v4/trial-license/prev",
            Some(admin.as_str()),
            403,
            "api.license.upgrade_needed.app_error",
        ),
        (
            "/api/v4/saml/metadata",
            None,
            501,
            "api.admin.saml.not_available.app_error",
        ),
        (
            "/api/v4/ldap/groups",
            Some(admin.as_str()),
            501,
            "ent.ldap.app_error",
        ),
        (
            "/api/v4/data_retention/policy",
            Some(admin.as_str()),
            501,
            "ent.data_retention.generic.license.error",
        ),
        (
            "/api/v4/data_retention/policies_count",
            Some(admin.as_str()),
            501,
            "ent.data_retention.generic.license.error",
        ),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            fetch_licensed_pair(&http, &pair, token, path).await;
        assert_eq!(go_status, status, "{path}: {}", text(&go));
        assert_eq!(rs_status, go_status, "{path}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, path);
        assert_eq!(parsed["id"], id, "{path}");
    }
}

// ---------------------------------------------------------------------------------------------
// Served both sides of the gate
// ---------------------------------------------------------------------------------------------

/// `/system/ping` never needed the licence: `ActiveSearchBackend` is decided by the
/// Elasticsearch settings, and both are off. Byte-identical on the licensed pair.
#[tokio::test]
async fn ping_is_served_and_identical_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, None, "/api/v4/system/ping").await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(text(&go), text(&rs));
    assert!(text(&go).contains(r#""ActiveSearchBackend":"database""#));
}

/// `getCPAGroup`: `MinimumEnterpriseLicense` holds on the pair, and the answer is the
/// `access_control` property group's id through `json.NewEncoder` — with its newline.
#[tokio::test]
async fn the_cpa_group_id_is_served_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let ((go_status, go), (rs_status, rs)) = fetch_licensed_pair(
        &http,
        &pair,
        Some(&admin),
        "/api/v4/custom_profile_attributes/group",
    )
    .await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200);
    assert_eq!(go, rs);
    let parsed: serde_json::Value = serde_json::from_slice(&go).unwrap();
    assert_eq!(parsed["id"].as_str().map(str::len), Some(26));
    assert!(go.ends_with(b"\n"), "json.NewEncoder writes a newline");
}

/// `getLicenseLoadMetric` with `Features.Users` set: `round(MAU / users * 1000)` over a
/// 31-day window, the same number from both, and a database read on our side to get it.
#[tokio::test]
async fn the_load_metric_is_computed_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, Some(&admin), "/api/v4/license/load_metric").await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200);
    assert_eq!(text(&go), text(&rs));
    let parsed: serde_json::Value = serde_json::from_slice(&go).unwrap();
    assert!(parsed["load"].is_i64(), "{}", text(&go));
}

/// `importTeam`'s first statement is `License().IsCloud()`, which the oracle's licence fails,
/// so a licensed request reaches the parse error like an unlicensed one — served, not forwarded.
#[tokio::test]
async fn import_team_passes_the_cloud_gate_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let path = format!("/api/v4/teams/{team}/import");
    // A multipart content type with no boundary: `ParseMultipartForm` fails on both.
    let (go_status, go, _) = send(
        &http,
        &pair.go,
        reqwest::Method::POST,
        Some(&admin),
        &path,
        "multipart/form-data",
        b"",
    )
    .await;
    let (rs_status, rs, by_rust) = send(
        &http,
        &pair.rust,
        reqwest::Method::POST,
        Some(&admin),
        &path,
        "multipart/form-data",
        b"",
    )
    .await;
    assert!(by_rust, "a non-cloud licence is served, not forwarded");
    assert_eq!(go_status, 500, "{}", text(&go));
    assert_eq!(rs_status, go_status);
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(parsed["id"], "api.team.import_team.parse.app_error");
}

/// `updateUserRoles` with a "new system role": `Features.CustomPermissionsSchemes` is on for
/// the oracle, so the gate passes and the write goes through on both servers — a 200 and
/// `{"status":"OK"}`. The unlicensed pair's 400 is pinned by `user_updates`.
#[tokio::test]
async fn roles_needing_custom_schemes_are_written_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let plain = create_plain_user(&http, &admin, &team, "licroles").await;
    let path = format!("/api/v4/users/{}/roles", plain.id);
    let ((go_status, go), (rs_status, rs)) = send_pair(
        &http,
        &pair,
        reqwest::Method::PUT,
        Some(&admin),
        &path,
        br#"{"roles":"system_user system_manager"}"#,
    )
    .await;
    delete_plain_user(&http, &admin, &plain.id).await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200, "{}", text(&rs));
    assert_eq!(text(&go), text(&rs));
    assert_eq!(text(&go), "{\"status\":\"OK\"}");
}

/// `switchAccountType`: with a licence, `ExperimentalEnableAuthenticationTransfer` (on, the
/// default) decides whether the branch is a 403; on it is not, and both branches fall through to
/// the account-type refusal an email/password user gets — served, and the same on both.
#[tokio::test]
async fn the_authentication_transfer_gate_passes_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let plain = create_plain_user(&http, &admin, &team, "licswitch").await;
    let me: serde_json::Value = http
        .get(format!("{GO}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {}", plain.token))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("a user");
    let email = me["email"].as_str().unwrap().to_owned();

    for (body, id) in [
        (
            format!(
                r#"{{"current_service":"gitlab","new_service":"email","email":"{email}","new_password":"Sw1tch-Pass-1234"}}"#
            ),
            "api.user.oauth_to_email.not_oauth_user.app_error",
        ),
        (
            format!(
                r#"{{"current_service":"ldap","new_service":"email","email":"{email}","password":"x","new_password":"Sw1tch-Pass-1234"}}"#
            ),
            "api.user.ldap_to_email.not_ldap_account.app_error",
        ),
    ] {
        let ((go_status, go), (rs_status, rs)) = send_pair(
            &http,
            &pair,
            reqwest::Method::POST,
            Some(&plain.token),
            "/api/v4/users/login/switch",
            body.as_bytes(),
        )
        .await;
        assert_eq!(go_status, 400, "{id}: {}", text(&go));
        assert_eq!(rs_status, go_status, "{id}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, id);
        assert_eq!(parsed["id"], id);
    }
    delete_plain_user(&http, &admin, &plain.id).await;
}

/// `postPriorityCheck` licensed: the tier gate passes, and the checks **behind** it are now the
/// ones that answer. `persistent_notifications` on a non-urgent post is the 400
/// `urgent_persistent_notification_post` — served — and a plain `requested_ack` passes every
/// check and reaches the create, whose `PostsPriority` write is not ported, so that one is
/// forwarded and Go's 201 comes back.
#[tokio::test]
async fn a_priority_post_passes_the_tier_gate_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (_, channel) = a_team_and_channel_the_user_is_in(&http, &admin).await;

    let body = format!(
        r#"{{"channel_id":"{channel}","message":"licensed priority","metadata":{{"priority":{{"priority":"important","persistent_notifications":true}}}}}}"#
    );
    let ((go_status, go), (rs_status, rs)) = send_pair(
        &http,
        &pair,
        reqwest::Method::POST,
        Some(&admin),
        "/api/v4/posts",
        body.as_bytes(),
    )
    .await;
    assert_eq!(go_status, 400, "{}", text(&go));
    assert_eq!(rs_status, 400, "{}", text(&rs));
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, "persistent non-urgent");
    assert_eq!(
        parsed["id"],
        "api.post.post_priority.urgent_persistent_notification_post.request_error"
    );

    let body = format!(
        r#"{{"channel_id":"{channel}","message":"licensed ack request","metadata":{{"priority":{{"priority":"important","requested_ack":true}}}}}}"#
    );
    let (rs_status, rs, by_rust) = send(
        &http,
        &pair.rust,
        reqwest::Method::POST,
        Some(&admin),
        "/api/v4/posts",
        "application/json",
        body.as_bytes(),
    )
    .await;
    assert!(
        !by_rust,
        "the gate passed and the PostsPriority write is Go's, so this is forwarded"
    );
    assert_eq!(rs_status, 201, "{}", text(&rs));
    let created: serde_json::Value = serde_json::from_slice(&rs).unwrap();
    assert_eq!(created["metadata"]["priority"]["requested_ack"], true);
    let id = created["id"].as_str().unwrap();
    let _ = http
        .delete(format!("{GO}/api/v4/posts/{id}"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await;
}

/// `getRecommendedChannelsForTeam` below the Advanced tier is `[]` on every server: the gate is
/// `MinimumEnterpriseAdvancedLicense && EnableAttributeBasedAccessControl`, and the oracle is
/// Enterprise, one rung short.
#[tokio::test]
async fn recommended_channels_are_served_below_the_advanced_tier() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let path = format!("/api/v4/teams/{team}/channels/recommended");
    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, Some(&admin), &path).await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200);
    assert_eq!(go, b"[]\n");
    assert_eq!(rs, go);
}

/// `getChannelModerations` past the gate: the four scheme roles resolved and folded into the
/// five moderations, byte for byte. A plain user is a 403 naming the console permission.
#[tokio::test]
async fn channel_moderations_are_served_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "licmod").await;
    let channel = create_channel(&http, &admin, &team, "licmod").await;
    let plain = create_plain_user(&http, &admin, &team, "licmod").await;
    let path = format!("/api/v4/channels/{channel}/moderations");

    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, Some(&admin), &path).await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200, "{}", text(&rs));
    assert_eq!(text(&go), text(&rs));
    let parsed: serde_json::Value = serde_json::from_slice(&go).unwrap();
    let names: Vec<&str> = parsed
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "create_post",
            "create_reactions",
            "manage_members",
            "use_channel_mentions",
            "manage_bookmarks"
        ]
    );
    assert!(parsed[2]["roles"].get("guests").is_none() || parsed[2]["roles"]["guests"].is_null());
    assert!(!go.ends_with(b"\n"), "json.Marshal + w.Write: no newline");

    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, Some(&plain.token), &path).await;
    assert_eq!(go_status, 403, "{}", text(&go));
    assert_eq!(rs_status, 403);
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
    assert_eq!(parsed["id"], "api.context.permissions.app_error");

    // And an id that is not one: the licence gate is first, `RequireChannelId` second.
    let ((go_status, go), (rs_status, rs)) = fetch_licensed_pair(
        &http,
        &pair,
        Some(&admin),
        "/api/v4/channels/abc/moderations",
    )
    .await;
    assert_eq!(go_status, 400, "{}", text(&go));
    assert_eq!(rs_status, 400);
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, "abc");
    assert_eq!(parsed["id"], "api.context.invalid_url_param.app_error");

    delete_plain_user(&http, &admin, &plain.id).await;
    delete_channel(&http, &admin, &channel).await;
}

/// `channelMemberCountsByGroup` past the gate: one planted group with two members in the
/// channel, one of them with a manual timezone, so `include_timezones` changes the answer and
/// the row order of a one-group result cannot hide a mismatch.
#[tokio::test]
async fn member_counts_by_group_are_served_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = create_team(&http, &admin, "licmcbg").await;
    let channel = create_channel(&http, &admin, &team, "licmcbg").await;
    let a = create_plain_user(&http, &admin, &team, "licmcbga").await;
    let b = create_plain_user(&http, &admin, &team, "licmcbgb").await;
    for user in [&a, &b] {
        let response = http
            .post(format!("{GO}/api/v4/channels/{channel}/members"))
            .header("Authorization", format!("Bearer {admin}"))
            .json(&serde_json::json!({ "user_id": user.id }))
            .send()
            .await
            .expect("Go answers");
        assert!(response.status().is_success());
    }
    let Ok(url) = std::env::var("DATABASE_URL") else {
        panic!("DATABASE_URL is set under the harness");
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");
    const GROUP: &str = "mmrslicmcbggroup0000000001";
    sqlx::query("DELETE FROM groupmembers WHERE groupid = $1")
        .bind(GROUP)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM usergroups WHERE id = $1")
        .bind(GROUP)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO usergroups
           (id, name, displayname, description, source, remoteid,
            createat, updateat, deleteat, allowreference)
         VALUES ($1, 'mmrs-licmcbg', 'Display mmrs-licmcbg', 'a group this suite made', 'custom',
                 NULL, 1700000000000, 1700000000000, 0, TRUE)",
    )
    .bind(GROUP)
    .execute(&pool)
    .await
    .unwrap();
    for user in [&a, &b] {
        sqlx::query(
            "INSERT INTO groupmembers (groupid, userid, createat, deleteat)
             VALUES ($1, $2, 1700000000000, 0)",
        )
        .bind(GROUP)
        .bind(&user.id)
        .execute(&pool)
        .await
        .unwrap();
    }
    // A **removed** membership for the outsider, who is not in the channel anyway but whose row
    // would be counted by a query that forgot `GroupMembers.DeleteAt = 0` if they were — so
    // they are added to the channel too. Two live members, one dead row: the count must stay 2.
    let outsider = create_plain_user(&http, &admin, &team, "licmcbgo").await;
    let response = http
        .post(format!("{GO}/api/v4/channels/{channel}/members"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "user_id": outsider.id }))
        .send()
        .await
        .expect("Go answers");
    assert!(response.status().is_success());
    sqlx::query(
        "INSERT INTO groupmembers (groupid, userid, createat, deleteat)
         VALUES ($1, $2, 1700000000000, 1700000001000)",
    )
    .bind(GROUP)
    .bind(&outsider.id)
    .execute(&pool)
    .await
    .unwrap();
    // One member on a manual timezone, so the distinct-timezone count is 1 and not 0.
    sqlx::query(
        r#"UPDATE users SET timezone = '{"useAutomaticTimezone":"false","manualTimezone":"Asia/Kolkata","automaticTimezone":""}'::jsonb WHERE id = $1"#,
    )
    .bind(&a.id)
    .execute(&pool)
    .await
    .unwrap();

    let path = format!("/api/v4/channels/{channel}/member_counts_by_group");
    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, Some(&admin), &path).await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200, "{}", text(&rs));
    assert_eq!(text(&go), text(&rs));
    let parsed: serde_json::Value = serde_json::from_slice(&go).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!([{ "group_id": GROUP, "channel_member_count": 2, "channel_member_timezones_count": 0 }])
    );

    let with_tz = format!("{path}?include_timezones=true");
    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, Some(&admin), &with_tz).await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200, "{}", text(&rs));
    assert_eq!(text(&go), text(&rs));
    let parsed: serde_json::Value = serde_json::from_slice(&go).unwrap();
    assert_eq!(parsed[0]["channel_member_timezones_count"], 1);
    assert!(!go.ends_with(b"\n"));

    // A stranger: `read_channel` refuses with the permission error, after the licence gate.
    let stranger = create_plain_user(&http, &admin, &team, "licmcbgs").await;
    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, Some(&stranger.token), &path).await;
    assert_eq!(go_status, 403, "{}", text(&go));
    assert_eq!(rs_status, 403);
    assert_error_bodies_match_except_known_gaps(&go, &rs, &path);

    sqlx::query("DELETE FROM groupmembers WHERE groupid = $1")
        .bind(GROUP)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM usergroups WHERE id = $1")
        .bind(GROUP)
        .execute(&pool)
        .await
        .unwrap();
    for user in [&a, &b, &outsider, &stranger] {
        delete_plain_user(&http, &admin, &user.id).await;
    }
    delete_channel(&http, &admin, &channel).await;
}

/// `createChannel` licensed: the only licence question is the anonymous-URL conjunction, which
/// needs the Advanced tier and a setting that is off — so a licensed create is served, and its
/// body matches the oracle's create modulo the per-channel fields.
///
/// The display names carry the per-run tag: `DELETE /channels/{id}` only archives, and two
/// archived channels sharing one display name are a sort tie that `channel_search_all`'s
/// `include_deleted` listings then order differently on the two servers (measured, once).
#[tokio::test]
async fn create_channel_is_served_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let tag = format!("mmrs-liccreate-{}", admin[..6].to_lowercase());
    let mut ids = Vec::new();
    let mut bodies = Vec::new();
    for (base, suffix, ours) in [(&pair.go, "go", false), (&pair.rust, "rs", true)] {
        let body = format!(
            r#"{{"team_id":"{team}","name":"{tag}-{suffix}","display_name":"{tag} {suffix}","type":"O"}}"#
        );
        let (status, raw, by_rust) = send(
            &http,
            base,
            reqwest::Method::POST,
            Some(&admin),
            "/api/v4/channels",
            "application/json",
            body.as_bytes(),
        )
        .await;
        assert_eq!(status, 201, "{suffix}: {}", text(&raw));
        assert_eq!(by_rust, ours, "{suffix}");
        let mut parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        ids.push(parsed["id"].as_str().unwrap().to_owned());
        let object = parsed.as_object_mut().unwrap();
        for key in ["id", "name", "display_name", "create_at", "update_at"] {
            object.insert(key.to_owned(), serde_json::json!("<per-channel>"));
        }
        bodies.push(parsed);
    }
    for id in &ids {
        delete_channel(&http, &admin, id).await;
    }
    assert_eq!(bodies[0], bodies[1]);
}

// ---------------------------------------------------------------------------------------------
// Forwarded on the precise predicate
// ---------------------------------------------------------------------------------------------

/// The scheme writes: the licence test is `CustomPermissionsSchemes || sku == professional`,
/// which the oracle passes, so a licensed request is handed to Go — and Go's own 400 for an
/// invalid scheme comes back. The unlicensed 501 is pinned by `schemes`.
#[tokio::test]
async fn scheme_writes_pass_the_gate_and_are_forwarded_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (go_status, go, _) = send(
        &http,
        &pair.go,
        reqwest::Method::POST,
        Some(&admin),
        "/api/v4/schemes",
        "application/json",
        b"{}",
    )
    .await;
    let (rs_status, rs, by_rust) = send(
        &http,
        &pair.rust,
        reqwest::Method::POST,
        Some(&admin),
        "/api/v4/schemes",
        "application/json",
        b"{}",
    )
    .await;
    assert!(!by_rust, "past the gate the write is Go's");
    assert_eq!(go_status, 400, "{}", text(&go));
    assert_eq!(rs_status, go_status);
    let parsed: serde_json::Value = serde_json::from_slice(&go).unwrap();
    let ours: serde_json::Value = serde_json::from_slice(&rs).unwrap();
    assert_eq!(parsed["id"], ours["id"]);
    assert_ne!(parsed["id"], "api.scheme.create_scheme.license.error");
}

/// The unlicensed pair is unmoved by any of this — one route per family, still served and still
/// the unlicensed answer. Held under the shared lock like every unlicensed assertion.
#[tokio::test]
async fn the_unlicensed_pair_still_refuses() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    for (path, status) in [
        ("/api/v4/custom_profile_attributes/group", 403),
        ("/api/v4/ldap/groups", 501),
        ("/api/v4/trial-license/prev", 403),
    ] {
        let ((go_status, go), (rs_status, rs)) = common::fetch_both_raw(&http, &admin, path).await;
        assert_eq!(go_status, status, "{path}: {}", text(&go));
        assert_eq!(rs_status, go_status, "{path}");
        assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    }
}

// ---------------------------------------------------------------------------------------------
// The client configuration's licensed blocks
// ---------------------------------------------------------------------------------------------

/// The one key that legitimately differs on the licensed pair: `BuildEnterpriseReady` is an
/// `-ldflags` constant, `"true"` in the enterprise-ready oracle and `""` in this binary, which
/// is not rebuilt per edition. Everything else in both maps is compared byte for byte.
const BUILD_FLAVOUR_KEY: &str = "BuildEnterpriseReady";

fn without_build_flavour(body: &[u8]) -> serde_json::Value {
    let mut parsed: serde_json::Value = serde_json::from_slice(body).expect("a JSON map");
    let removed = parsed.as_object_mut().unwrap().remove(BUILD_FLAVOUR_KEY);
    assert!(removed.is_some(), "{BUILD_FLAVOUR_KEY} is in the map");
    parsed
}

/// `GET /config/client` licensed, anonymous and as an administrator: the limited map with Go's
/// `if license != nil` block, and the full map with its feature matrix — the Enterprise
/// oracle's licence turns every flag on except `cloud`, so every arm below the Advanced tier is
/// exercised and the Advanced arm's keys are absent on both.
#[tokio::test]
async fn the_client_config_carries_the_licensed_blocks() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;

    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, None, "/api/v4/config/client?format=old").await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200, "{}", text(&rs));
    let go_limited = without_build_flavour(&go);
    let rs_limited = without_build_flavour(&rs);
    assert_eq!(go_limited, rs_limited, "the limited map, licensed");
    assert_eq!(
        go_limited["EnableLdap"], "false",
        "LDAP licensed, LdapSettings.Enable off"
    );
    assert!(go_limited.get("EnableCustomTermsOfService").is_some());
    assert!(
        go_limited.get("EnableSignUpWithGitLab").is_some(),
        "the OpenId arm adds GitLab"
    );
    assert!(
        go_limited.get("MobileEnableBiometrics").is_some(),
        "the Enterprise arm"
    );
    assert!(
        go_limited.get("IntuneMAMEnabled").is_none(),
        "the Advanced arm is closed"
    );

    let ((go_status, go), (rs_status, rs)) = fetch_licensed_pair(
        &http,
        &pair,
        Some(&admin),
        "/api/v4/config/client?format=old",
    )
    .await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200, "{}", text(&rs));
    let go_full = without_build_flavour(&go);
    let rs_full = without_build_flavour(&rs);
    assert_eq!(go_full, rs_full, "the full map, licensed");
    assert_eq!(
        go_full["PostAcknowledgements"], "true",
        "the Professional arm"
    );
    assert!(
        go_full.get("LockProfileFieldsForEmailUsers").is_some(),
        "the Enterprise arm"
    );
    assert!(
        go_full.get("EnableClientMetrics").is_some(),
        "the Cluster arm's metrics block"
    );
    assert_eq!(
        go_full["DataRetentionMessageRetentionHours"], "8760",
        "365 days, the default, in hours"
    );
    assert!(
        go_full.get("ContentFlaggingEnabled").is_none(),
        "the Advanced arm is closed"
    );
    assert!(
        go_full.get("ExperimentalSharedChannels").is_some(),
        "HasSharedChannels"
    );
}

/// `GET /config` and `GET /config/environment` licensed: neither asks the licence anything a
/// self-hosted licence changes — `getConfig` filters cloud-restrictable fields only for a cloud
/// licence — so both are served, and identical but for the per-checkout file directory.
#[tokio::test]
async fn the_config_and_environment_reads_are_served_licensed() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;

    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, Some(&admin), "/api/v4/config").await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200, "{}", text(&rs));
    let mut go: serde_json::Value = serde_json::from_slice(&go).unwrap();
    let mut rs: serde_json::Value = serde_json::from_slice(&rs).unwrap();
    for document in [&mut go, &mut rs] {
        document["FileSettings"]
            .as_object_mut()
            .unwrap()
            .remove("Directory")
            .expect("FileSettings.Directory is in the document");
    }
    assert_eq!(
        go, rs,
        "the configuration, licensed, but for the per-checkout directory"
    );

    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&http, &pair, Some(&admin), "/api/v4/config/environment").await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200, "{}", text(&rs));
    assert_eq!(text(&go), text(&rs), "the environment overlay, licensed");
}

/// `login` licensed: a self-hosted licence is served — a plain user logs in on both, and a
/// **guest** reaches Go's *second* refusal, `guest_accounts.disabled.error`, because the licence
/// gate in front of it passes and `GuestAccountsSettings.Enable` is off.
///
/// **Neither guest id reaches the wire.** Both are masked by `login`'s deferred error mask to
/// `invalid_credentials_email_username` (api4/user.go:2127) — so on the wire a licensed guest
/// refusal and an unlicensed one are the same bytes, and the arm this exercises is visible only
/// in the log. Asserted as what a client sees, not as what the handler chose.
#[tokio::test]
async fn login_is_served_licensed_and_a_guest_meets_the_second_refusal() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let http = client();
    let admin = go_minted_token(&http).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&http, &admin).await;
    let plain = create_plain_user(&http, &admin, &team, "liclogin").await;
    let me: serde_json::Value = http
        .get(format!("{GO}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {}", plain.token))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("a user");
    let email = me["email"].as_str().unwrap().to_owned();
    let body = serde_json::json!({ "login_id": email, "password": common::PLAIN_USER_PASSWORD });
    let body = serde_json::to_vec(&body).unwrap();

    let ((go_status, go), (rs_status, rs)) = send_pair(
        &http,
        &pair,
        reqwest::Method::POST,
        None,
        "/api/v4/users/login",
        &body,
    )
    .await;
    assert_eq!(go_status, 200, "{}", text(&go));
    assert_eq!(rs_status, 200, "{}", text(&rs));
    let go_user: serde_json::Value = serde_json::from_slice(&go).unwrap();
    let rs_user: serde_json::Value = serde_json::from_slice(&rs).unwrap();
    assert_eq!(go_user["id"], rs_user["id"]);

    // Make the account a guest directly, as the guest suites do, and log in again on both.
    let Ok(url) = std::env::var("DATABASE_URL") else {
        panic!("DATABASE_URL is set under the harness");
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");
    sqlx::query("UPDATE users SET roles = 'system_guest' WHERE id = $1")
        .bind(&plain.id)
        .execute(&pool)
        .await
        .unwrap();
    for base in [&pair.go, GO] {
        let _ = http
            .post(format!("{base}/api/v4/caches/invalidate"))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await;
    }
    let ((go_status, go), (rs_status, rs)) = send_pair(
        &http,
        &pair,
        reqwest::Method::POST,
        None,
        "/api/v4/users/login",
        &body,
    )
    .await;
    assert_eq!(go_status, 401, "{}", text(&go));
    assert_eq!(rs_status, 401, "{}", text(&rs));
    let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, "guest login, licensed");
    assert_eq!(
        parsed["id"], "api.user.login.invalid_credentials_email_username",
        "both guest refusals are masked to the same id"
    );

    sqlx::query("UPDATE users SET roles = 'system_user' WHERE id = $1")
        .bind(&plain.id)
        .execute(&pool)
        .await
        .unwrap();
    delete_plain_user(&http, &admin, &plain.id).await;
}
