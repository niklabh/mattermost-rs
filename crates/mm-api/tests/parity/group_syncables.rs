//! Cross-server parity for the three syncable writes in `api4/group.go`, and for the twenty-two
//! route+method pairs the group family now answers as a whole.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity parity::group_syncables
//! ```
//!
//! `linkGroupSyncable` (`POST .../link`), `unlinkGroupSyncable` (`DELETE .../link`) and
//! `patchGroupSyncable` (`PUT .../patch`) — the last three handlers `InitGroup` registers.
//!
//! # The gate precedes four things, not one
//!
//! All three open with `requireLicense` (api4/handlers.go:237) as their first statement, above
//! `RequireGroupId`, `RequireSyncableId`, `RequireSyncableType` **and** `io.ReadAll(r.Body)`. So
//! on an unlicensed server every combination of a bad id, a bad body and a missing body is one
//! 501 with `api.license_error`, and a port that validated or parsed first would answer four
//! different wrong statuses. That is [`the_licence_gate_precedes_the_ids_and_the_body`].
//!
//! # Two literals at a fourth segment, and the proof they shadow nothing
//!
//! `/groups/names` cost this project a served route once: axum prefers a static segment over
//! `{group_id}` and does not backtrack across method routers, so registering the literal for
//! `POST` alone silently handed `GET /groups/names` to the fallback. `link` and `patch` here sit
//! two parameters deeper, where nothing was registered — but that is an argument about matchit,
//! not a measurement, so [`every_group_route_this_server_answered_still_answers`] re-asks all
//! twenty-two pairs and fails if any one of them started being forwarded.
//!
//! # `syncable_type` is an alternation of literals, and `RequireSyncableType` is dead code
//!
//! The route pattern is `{syncable_type:teams|channels}` (group.go:39), so a third value never
//! matched gorilla and Go answers its own mux 404 — not the licence error, even though the gate
//! would be first if the request ever reached a handler. Behind that, `params.go:269` maps
//! `teams` → `Team` and `channels` → `Channel`, which means `RequireSyncableType`'s
//! `SetInvalidURLParam` branch cannot be reached through the mux at all.
//! [`a_third_syncable_type_is_forwarded`] measures the routing half; the dead branch is recorded
//! and not tested, because no request can produce it.
//!
//! # What this suite cannot reach
//!
//! Behind the gate sit `verifyLinkUnlinkPermission`, `verifySchemeAdminAssignmentPermission`, the
//! `GroupSyncable` upsert surface and the asynchronous `SyncRolesAndMembership`. Go loads its
//! licence at startup and re-reads it only on a save, so `set_active_licence_id` moves *our*
//! answer and not Go's — which is why [`a_licence_row_hands_every_syncable_write_back_to_go`] can
//! assert the forwarding boundary and nothing past it. See [D-390].

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, assert_error_bodies_match_except_known_gaps, client,
    go_minted_token, set_active_licence_id, stack_enabled,
};

/// A 26-character id that names nothing. The gate refuses before anything looks it up, so no
/// fixture group, team or channel is needed anywhere in this suite — which is also why nothing
/// here can disturb the seeded memberships other suites assert on.
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// One request to one server, returning `(status, body, x-mmrs-served-by)`.
async fn send(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: &str,
) -> (u16, Vec<u8>, Option<String>) {
    let response = client
        .request(method.clone(), format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} {method} {path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    (
        status,
        response.bytes().await.expect("body reads").to_vec(),
        served_by,
    )
}

/// Compare two error bodies that are **both Go's**, tolerating only `request_id`.
///
/// [`assert_error_bodies_match_except_known_gaps`] also tolerates `message`, because a body we
/// mint carries the untranslated error id where Go's carries prose ([D-092]). A *forwarded* body
/// has been through Go's i18n on both sides, so that allowance would hide a real difference.
fn assert_forwarded_bodies_match(go_body: &[u8], rs_body: &[u8], context: &str) {
    let mut go: serde_json::Value = serde_json::from_slice(go_body)
        .unwrap_or_else(|e| panic!("{context}: Go's body is not JSON: {e}"));
    let mut rs: serde_json::Value = serde_json::from_slice(rs_body)
        .unwrap_or_else(|e| panic!("{context}: the forwarded body is not JSON: {e}"));
    for v in [&mut go, &mut rs] {
        if let Some(map) = v.as_object_mut() {
            map.remove("request_id");
        }
    }
    assert_eq!(go, rs, "{context}: a forwarded body is Go's own");
}

/// The three handlers, across **both** syncable types — six pairs, spelled out rather than
/// generated. The two arms have different permission gates and different response shapes behind
/// the licence, so a suite that exercised only `teams` would be asserting half the surface.
fn every_syncable_write(
    group_id: &str,
    syncable_type: &str,
    syncable_id: &str,
) -> Vec<(reqwest::Method, String, &'static str)> {
    vec![
        (
            reqwest::Method::POST,
            format!("/api/v4/groups/{group_id}/{syncable_type}/{syncable_id}/link"),
            r#"{"auto_add":true}"#,
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/groups/{group_id}/{syncable_type}/{syncable_id}/link"),
            "",
        ),
        (
            reqwest::Method::PUT,
            format!("/api/v4/groups/{group_id}/{syncable_type}/{syncable_id}/patch"),
            r#"{"auto_add":true,"scheme_admin":true}"#,
        ),
    ]
}

/// All three handlers, both syncable types, each answering the generic 501 — and each served by
/// **us** rather than forwarded, without which the comparison would be Go against Go and would
/// pass whatever the router did.
#[tokio::test]
async fn every_syncable_write_answers_the_generic_licence_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let mut pairs = 0;
    for syncable_type in ["teams", "channels"] {
        let routes = every_syncable_write(NOWHERE, syncable_type, NOWHERE);
        assert_eq!(routes.len(), 3, "three handlers per syncable type");

        for (method, path, body) in &routes {
            let label = format!("{method} {path}");
            let (go_status, go, _) = send(&client, GO, &token, method.clone(), path, body).await;
            let (rs_status, rs, served_by) =
                send(&client, RUST, &token, method.clone(), path, body).await;

            assert_eq!(
                served_by.as_deref(),
                Some("rust"),
                "{label} was forwarded, so this proves nothing about the Rust handler"
            );
            assert_eq!(
                go_status, 501,
                "{label}: `requireLicense` is a 501, not a 403"
            );
            assert_eq!(rs_status, go_status, "{label}");
            let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &label);
            assert_eq!(
                parsed["id"], "api.license_error",
                "{label}: the generic id, shared by every group route"
            );
            assert!(
                !rs.ends_with(b"\n"),
                "{label}: error bodies carry no newline"
            );
            pairs += 1;
        }
    }
    assert_eq!(pairs, 6, "three handlers times two syncable types");
}

/// **The gate precedes `RequireGroupId`, `RequireSyncableId` and the body, in that order.**
///
/// Every combination of a routable-but-invalid id and an unparseable body is the same 501. Go
/// reads the body with `io.ReadAll` *below* three `Require*` calls that are themselves below the
/// licence check, so there are four places a helpful port could answer early and all four are
/// wrong.
#[tokio::test]
async fn the_licence_gate_precedes_the_ids_and_the_body() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // `abc` and `0` pass `[A-Za-z0-9]+` and so are routed, but fail `IsValidId` in the handler —
    // which never runs, because the gate is above it.
    let ids = ["abc", "0", NOWHERE];
    let bodies = [
        ("malformed", "{"),
        ("empty", ""),
        ("json_null", "null"),
        ("wrong_shape", r#"{"auto_add":42}"#),
        ("array_where_object_expected", "[1,2,3]"),
    ];

    for syncable_type in ["teams", "channels"] {
        for group_id in ids {
            for syncable_id in ids {
                for (what, body) in &bodies {
                    for (method, path, _) in
                        every_syncable_write(group_id, syncable_type, syncable_id)
                    {
                        let label = format!("{method} {path} [{what}]");
                        let (go_status, go, _) =
                            send(&client, GO, &token, method.clone(), &path, body).await;
                        let (rs_status, rs, served_by) =
                            send(&client, RUST, &token, method.clone(), &path, body).await;

                        assert_eq!(served_by.as_deref(), Some("rust"), "{label} was forwarded");
                        assert_eq!(
                            go_status, 501,
                            "{label}: Go refuses for the licence, not for the id or the body"
                        );
                        assert_eq!(rs_status, go_status, "{label}");
                        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &label);
                        assert_eq!(parsed["id"], "api.license_error", "{label}");
                        assert_ne!(
                            parsed["id"], "api.context.invalid_url_param.app_error",
                            "{label}: not the id error"
                        );
                    }
                }
            }
        }
    }
}

/// **A third `syncable_type` never matched gorilla**, so it is Go's own mux 404 — forwarded, not
/// answered with the licence error. The pattern is `teams|channels`, an alternation of two
/// literals: case matters, and so does the plural.
#[tokio::test]
async fn a_third_syncable_type_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // `team`/`channel` are the singular forms a reader might expect from the *model* constants;
    // `Teams`/`Channels` are the cased forms `GroupSyncableType` actually uses. All four are mux
    // 404s, and all four are the mistake this guards.
    for syncable_type in ["team", "channel", "Teams", "Channels", "foo", "members"] {
        for (method, path, body) in every_syncable_write(NOWHERE, syncable_type, NOWHERE) {
            let label = format!("{method} {path}");
            let (go_status, go, _) = send(&client, GO, &token, method.clone(), &path, body).await;
            let (rs_status, rs, served_by) =
                send(&client, RUST, &token, method.clone(), &path, body).await;

            assert_eq!(
                served_by.as_deref(),
                Some("go"),
                "{label} must be forwarded so Go answers its own 404"
            );
            assert_eq!(go_status, 404, "{label}: gorilla's NotFoundHandler");
            assert_eq!(rs_status, go_status, "{label}");
            assert_forwarded_bodies_match(&go, &rs, &label);
        }
    }
}

/// **A `group_id` or `syncable_id` outside `[A-Za-z0-9]+` never matched gorilla**, so it is Go's
/// own mux 404 — forwarded. Both parameters are `_id`-shaped and both carry the class, so this
/// asks each of them separately: a middleware that checked only the first would pass a
/// single-parameter test and fail here.
#[tokio::test]
async fn an_id_outside_the_mux_charset_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    // A hyphen, an underscore and a dot are all outside the class — and all three are legal in a
    // group *name*, which is what makes this the mistake worth guarding.
    for bad in ["not-an-id", "not_an_id", "not.an.id"] {
        for (group_id, syncable_id) in [(bad, NOWHERE), (NOWHERE, bad), (bad, bad)] {
            for (method, path, body) in every_syncable_write(group_id, "teams", syncable_id) {
                let label = format!("{method} {path}");
                let (go_status, go, _) =
                    send(&client, GO, &token, method.clone(), &path, body).await;
                let (rs_status, rs, served_by) =
                    send(&client, RUST, &token, method.clone(), &path, body).await;

                assert_eq!(
                    served_by.as_deref(),
                    Some("go"),
                    "{label} must be forwarded so Go answers its own 404"
                );
                assert_eq!(go_status, 404, "{label}: gorilla's NotFoundHandler");
                assert_eq!(rs_status, go_status, "{label}");
                // Compared **raw**: gorilla's `NotFoundHandler` answers before the middleware
                // that mints `X-Request-Id`, so this body has no per-request field and two
                // separate requests produce identical bytes.
                assert_eq!(
                    String::from_utf8_lossy(&go),
                    String::from_utf8_lossy(&rs),
                    "{label}: a forwarded body is Go's own"
                );
            }
        }
    }
}

/// Methods gorilla registers on **neither** path must still reach Go rather than axum's 405.
///
/// `/link` carries `POST` and `DELETE` and nothing else; `/patch` carries `PUT` alone. Claiming
/// two methods on one path must not claim the rest of them, and the `PUT` on `/link` is the
/// specific confusion worth measuring: the sibling path answers `PUT`, this one does not.
#[tokio::test]
async fn unregistered_methods_still_forward() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let link = format!("/api/v4/groups/{NOWHERE}/teams/{NOWHERE}/link");
    let patch = format!("/api/v4/groups/{NOWHERE}/teams/{NOWHERE}/patch");

    for (method, path) in [
        (reqwest::Method::GET, link.clone()),
        (reqwest::Method::PUT, link.clone()),
        (reqwest::Method::PATCH, link),
        (reqwest::Method::GET, patch.clone()),
        (reqwest::Method::POST, patch.clone()),
        (reqwest::Method::DELETE, patch),
    ] {
        let label = format!("{method} {path}");
        let (go_status, go, _) = send(&client, GO, &token, method.clone(), &path, "").await;
        let (rs_status, rs, served_by) =
            send(&client, RUST, &token, method.clone(), &path, "").await;
        assert_eq!(
            served_by.as_deref(),
            Some("go"),
            "{label} must be forwarded, not answered with our 405"
        );
        assert_eq!(rs_status, go_status, "{label}");
        assert_forwarded_bodies_match(&go, &rs, &label);
    }
}

/// Every route+method pair in the group family that this server answered before the two syncable
/// paths were registered, plus the three that were added — **all twenty-two re-asked**.
///
/// This is the [D-330] hazard measured rather than argued: a static segment shadows its
/// parameterised sibling for every method, and the failure is silent. `link` and `patch` sit at a
/// fourth segment under two parameters, which *should* collide with nothing; the test is what
/// makes that a fact. A pair that starts being forwarded fails here naming itself.
#[tokio::test]
async fn every_group_route_this_server_answered_still_answers() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let g = NOWHERE;
    let pairs: Vec<(reqwest::Method, String, &str)> = vec![
        // The nineteen that were served before this session.
        (reqwest::Method::GET, "/api/v4/groups".to_owned(), ""),
        (
            reqwest::Method::POST,
            "/api/v4/groups".to_owned(),
            r#"{"name":"parity.group"}"#,
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/users/{g}/groups"),
            "",
        ),
        (
            reqwest::Method::POST,
            "/api/v4/groups/names".to_owned(),
            "[]",
        ),
        (reqwest::Method::GET, "/api/v4/groups/names".to_owned(), ""),
        (
            reqwest::Method::DELETE,
            "/api/v4/groups/names".to_owned(),
            "",
        ),
        (reqwest::Method::GET, format!("/api/v4/groups/{g}"), ""),
        (reqwest::Method::DELETE, format!("/api/v4/groups/{g}"), ""),
        (
            reqwest::Method::PUT,
            format!("/api/v4/groups/{g}/patch"),
            "{}",
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/groups/{g}/restore"),
            "",
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/groups/{g}/members"),
            "",
        ),
        (
            reqwest::Method::POST,
            format!("/api/v4/groups/{g}/members"),
            r#"{"user_ids":[]}"#,
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/groups/{g}/members"),
            r#"{"user_ids":[]}"#,
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/groups/{g}/stats"),
            "",
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/groups/{g}/teams"),
            "",
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/groups/{g}/channels/{g}"),
            "",
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/channels/{g}/groups"),
            "",
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/teams/{g}/groups"),
            "",
        ),
        (
            reqwest::Method::GET,
            format!("/api/v4/teams/{g}/groups_by_channels"),
            "",
        ),
        // The three added here.
        (
            reqwest::Method::POST,
            format!("/api/v4/groups/{g}/teams/{g}/link"),
            r#"{"auto_add":true}"#,
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/v4/groups/{g}/channels/{g}/link"),
            "",
        ),
        (
            reqwest::Method::PUT,
            format!("/api/v4/groups/{g}/teams/{g}/patch"),
            r#"{"auto_add":true}"#,
        ),
    ];
    assert_eq!(
        pairs.len(),
        22,
        "twenty of `InitGroup`'s handlers plus the two methods re-claimed at the `/names` literal"
    );

    for (method, path, body) in &pairs {
        let label = format!("{method} {path}");
        let (go_status, go, _) = send(&client, GO, &token, method.clone(), path, body).await;
        let (rs_status, rs, served_by) =
            send(&client, RUST, &token, method.clone(), path, body).await;

        assert_eq!(
            served_by.as_deref(),
            Some("rust"),
            "{label} is forwarded — a registration took it out of service"
        );
        assert_eq!(go_status, 501, "{label}");
        assert_eq!(rs_status, go_status, "{label}");
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, &label);
        assert_eq!(parsed["id"], "api.license_error", "{label}");
    }
}

/// The boundary: a licence row hands all three syncable writes back to the proxy, and removing it
/// takes them back.
#[tokio::test]
async fn a_licence_row_hands_every_syncable_write_back_to_go() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let routes = every_syncable_write(NOWHERE, "teams", NOWHERE);

    set_active_licence_id(Some("mmrslicence000000000000004")).await;
    let mut forwarded = Vec::new();
    for (method, path, body) in &routes {
        let (_, _, served_by) = send(&client, RUST, &token, method.clone(), path, body).await;
        forwarded.push((format!("{method} {path}"), served_by));
    }
    // Restored before any assertion, so a failure cannot leave the row set for another suite.
    set_active_licence_id(None).await;

    for (label, answer) in &forwarded {
        assert_eq!(answer.as_deref(), Some("go"), "{label} must be forwarded");
    }

    for (method, path, body) in &routes {
        let (_, _, served_by) = send(&client, RUST, &token, method.clone(), path, body).await;
        assert_eq!(
            served_by.as_deref(),
            Some("rust"),
            "{method} {path} after the row is cleared"
        );
    }
}
