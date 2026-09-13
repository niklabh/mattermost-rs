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
    ACTIVE_LICENCE_ROW, GO, LicensedPair, RUST, SocketProbe,
    assert_error_bodies_match_except_known_gaps, client, create_channel, create_plain_user,
    create_team, go_minted_token, licensed, set_active_licence_id, stack_enabled,
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

/// **The boundary, as `LoadLicense` draws it.** A `Systems.ActiveLicenseId` that names no
/// `Licenses` row is not a licence: Go looks the row up (platform/license.go:104) and finds
/// nothing, and since 2026-09-13 so do we. Planting one therefore changes nothing on the wire —
/// still served here, still the unlicensed answer. The licensed half is compared against the
/// licensed pair (`common::licensed`), never against this row. Holds the shared lock exclusively.
#[tokio::test]
async fn a_planted_id_without_a_row_is_not_a_licence() {
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
        assert_eq!(
            answer.as_deref(),
            Some("rust"),
            "{label}: an ActiveLicenseId naming no Licenses row is not a licence, so still ours"
        );
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

// ---------------------------------------------------------------------------------------------
// The licensed pair
//
// Everything above measures the one 501 an unlicensed server answers. Below, the same three
// routes on `common::licensed` — the enterprise-ready Go oracle and an mm-api carrying the same
// licence — where the handlers run to the end. The two servers share one database, so a link
// made through one is visible to the other and the membership sync of either writes rows both
// see; each side therefore gets **its own** group, team, channel and member, isomorphic to the
// other's, and the comparison is between the two outcomes rather than between two answers to one
// request.
// ---------------------------------------------------------------------------------------------

/// The four groups this half plants, by SQL — every route that would write them is the one under
/// test. `SYNCABLE_*` are LDAP-source (`IsSyncable`), one per side; `HIDDEN` hides its members
/// (`AllowReference = false`), which sends the request to Go from this side; `CUSTOM` is not
/// syncable at all.
const SYNCABLE_GO: &str = "mmrssyncgrpgo0000000000001";
const SYNCABLE_RS: &str = "mmrssyncgrprs0000000000001";
const HIDDEN: &str = "mmrssyncgrphidden000000001";
const CUSTOM: &str = "mmrssyncgrpcustom000000001";

struct LicensedFixture {
    pair: LicensedPair,
    admin: String,
    /// `(team, channel, group, member)` for the Go side and for the Rust side.
    go: Side,
    rs: Side,
    /// The team the plain users live in, which no test ever links — the "never linked" side of
    /// the refusals — and a channel in it, likewise.
    never_team: String,
    never_channel: String,
    /// A user in none of the fixture teams, so the team arm's permission question is answered
    /// by their system roles alone.
    outsider: common::PlainUser,
    /// A team admin of `go.team` and `rs.team` who is not a system admin, for the channel arm's
    /// team question.
    team_admin: common::PlainUser,
}

struct Side {
    base: String,
    team: String,
    channel: String,
    /// A second team for the membership sync, which ends by group-constraining it and unlinking
    /// — which removes **every** member not in a linked group, the administrator included. Done
    /// to `team`, that would silently stop Go delivering team-scoped websocket frames to the
    /// admin's session (`isMemberOfTeam`, web_conn.go:1029) and the events test would see
    /// nothing. Measured, before this field existed.
    constrained_team: String,
    group: &'static str,
    member: common::PlainUser,
}

/// The licensed tests share the fixture rows above and each walks a link through several states,
/// so they take turns: two of them racing on one `GroupTeams` row is a duplicate-key 500 on one
/// side and a "second unlink" that finds a live row on the other — measured, before this lock.
static LICENSED_TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static LICENSED_FIXTURE: tokio::sync::OnceCell<LicensedFixture> =
    tokio::sync::OnceCell::const_new();

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("the parity stack exports DATABASE_URL");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable")
}

async fn plant_group(pool: &sqlx::PgPool, id: &str, source: &str, allow_reference: bool) {
    assert_eq!(
        id.len(),
        26,
        "{id}: `IsValidId` and the column both want 26 characters"
    );
    sqlx::query(
        "INSERT INTO usergroups
           (id, name, displayname, description, source, remoteid, createat, updateat, deleteat,
            allowreference)
         VALUES ($1, $2, $3, 'a group the syncable suite made', $4, $5, 1700000000000,
                 1700000000000, 0, $6)",
    )
    .bind(id)
    .bind(format!("mmrs-sync-{}", &id[11..]))
    .bind(format!("Display {id}"))
    .bind(source)
    .bind(if source == "ldap" {
        Some(format!("remote-{id}"))
    } else {
        None
    })
    .bind(allow_reference)
    .execute(pool)
    .await
    .expect("the group is written");
}

async fn licensed_fixture(client: &reqwest::Client) -> &'static LicensedFixture {
    LICENSED_FIXTURE
        .get_or_init(|| async {
            let pair = licensed().await;
            let admin = go_minted_token(client).await;
            let pool = pool().await;
            for statement in [
                "DELETE FROM groupmembers WHERE groupid LIKE 'mmrssyncgrp%'",
                "DELETE FROM groupteams WHERE groupid LIKE 'mmrssyncgrp%'",
                "DELETE FROM groupchannels WHERE groupid LIKE 'mmrssyncgrp%'",
                "DELETE FROM usergroups WHERE id LIKE 'mmrssyncgrp%'",
            ] {
                sqlx::query(statement)
                    .execute(&pool)
                    .await
                    .expect("the sweep runs");
            }
            plant_group(&pool, SYNCABLE_GO, "ldap", true).await;
            plant_group(&pool, SYNCABLE_RS, "ldap", true).await;
            plant_group(&pool, HIDDEN, "ldap", false).await;
            plant_group(&pool, CUSTOM, "custom", true).await;

            let team_go = create_team(client, &admin, "syncgo").await;
            let team_rs = create_team(client, &admin, "syncrs").await;
            let constrained_go = create_team(client, &admin, "synccgo").await;
            let constrained_rs = create_team(client, &admin, "synccrs").await;
            let outside = create_team(client, &admin, "syncout").await;
            let channel_go = create_channel(client, &admin, &team_go, "syncgo").await;
            let channel_rs = create_channel(client, &admin, &team_rs, "syncrs").await;
            let never_channel = create_channel(client, &admin, &outside, "syncnever").await;

            // The members-to-be live in `outside` only, so an appearance in a fixture team can
            // only be the sync's doing.
            let member_go = create_plain_user(client, &admin, &outside, "syncmgo").await;
            let member_rs = create_plain_user(client, &admin, &outside, "syncmrs").await;
            let outsider = create_plain_user(client, &admin, &outside, "syncout").await;
            let team_admin = create_plain_user(client, &admin, &outside, "syncadm").await;
            for (group, member) in [(SYNCABLE_GO, &member_go), (SYNCABLE_RS, &member_rs)] {
                sqlx::query(
                    "INSERT INTO groupmembers (groupid, userid, createat, deleteat)
                     VALUES ($1, $2, 1700000000000, 0)",
                )
                .bind(group)
                .bind(&member.id)
                .execute(&pool)
                .await
                .expect("the membership is written");
            }
            for team in [&team_go, &team_rs, &outside] {
                let response = client
                    .post(format!("{GO}/api/v4/teams/{team}/members"))
                    .header("Authorization", format!("Bearer {admin}"))
                    .json(&serde_json::json!({ "team_id": team, "user_id": team_admin.id }))
                    .send()
                    .await
                    .expect("Go answers");
                assert!(response.status().is_success(), "the team admin joins");
                let response = client
                    .put(format!(
                        "{GO}/api/v4/teams/{team}/members/{}/schemeRoles",
                        team_admin.id
                    ))
                    .header("Authorization", format!("Bearer {admin}"))
                    .json(&serde_json::json!({ "scheme_user": true, "scheme_admin": true }))
                    .send()
                    .await
                    .expect("Go answers");
                assert!(response.status().is_success(), "the team admin is promoted");
            }
            // The oracle caches team and channel lookups it has already made; nothing here has
            // been asked of it yet, but a previous run's negative cache for these ids would be.
            let _ = client
                .post(format!("{}/api/v4/caches/invalidate", pair.go))
                .header("Authorization", format!("Bearer {admin}"))
                .send()
                .await;

            LicensedFixture {
                go: Side {
                    base: pair.go.clone(),
                    team: team_go,
                    channel: channel_go,
                    constrained_team: constrained_go,
                    group: SYNCABLE_GO,
                    member: member_go,
                },
                rs: Side {
                    base: pair.rust.clone(),
                    team: team_rs,
                    channel: channel_rs,
                    constrained_team: constrained_rs,
                    group: SYNCABLE_RS,
                    member: member_rs,
                },
                never_team: outside,
                never_channel,
                pair,
                admin,
                outsider,
                team_admin,
            }
        })
        .await
}

fn link(group: &str, kind: &str, id: &str) -> String {
    format!("/api/v4/groups/{group}/{kind}/{id}/link")
}

fn patch(group: &str, kind: &str, id: &str) -> String {
    format!("/api/v4/groups/{group}/{kind}/{id}/patch")
}

/// One request to one server of the pair, asserting the Rust side answered it itself unless
/// `forwarded` says a hand-over is the expected shape.
async fn licensed_send(
    client: &reqwest::Client,
    fx: &LicensedFixture,
    base: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: &str,
) -> (u16, Vec<u8>) {
    let (status, bytes, served_by) = send(client, base, token, method, path, body).await;
    if base == fx.pair.rust {
        assert_eq!(
            served_by.as_deref(),
            Some("rust"),
            "{path} was forwarded, so this proves nothing about the Rust handler"
        );
    }
    (status, bytes)
}

/// A syncable body with the per-side ids and the clock taken out, so the Go side's and the Rust
/// side's can be compared as shapes. Timestamps must be present and positive on both.
fn normalised_syncable(body: &[u8], side: &Side) -> serde_json::Value {
    let mut value: serde_json::Value =
        serde_json::from_slice(body).unwrap_or_else(|e| panic!("not JSON: {e}"));
    let map = value.as_object_mut().expect("an object");
    for key in ["team_id", "channel_id", "group_id"] {
        if let Some(v) = map.get_mut(key) {
            let s = v.as_str().expect("an id").to_owned();
            let placeholder = if s == side.team || s == side.constrained_team {
                "<team>"
            } else if s == side.channel {
                "<channel>"
            } else if s == side.group {
                "<group>"
            } else {
                panic!("{key} names {s}, which is none of this side's ids")
            };
            *v = serde_json::Value::String(placeholder.to_owned());
        }
    }
    // The display names carry the side's tag; presence and non-emptiness are what is compared.
    for key in ["team_display_name", "channel_display_name"] {
        if let Some(v) = map.get_mut(key) {
            assert!(
                !v.as_str().unwrap_or_default().is_empty(),
                "{key} is filled"
            );
            *v = serde_json::Value::String(format!("<{key}>"));
        }
    }
    for key in ["create_at", "update_at"] {
        let v = map
            .get_mut(key)
            .unwrap_or_else(|| panic!("{key} is present"));
        assert!(v.as_i64().unwrap_or(0) > 0, "{key} is a real timestamp");
        *v = serde_json::Value::from(0);
    }
    if let Some(v) = map.get_mut("delete_at") {
        if v.as_i64().unwrap_or(0) > 0 {
            *v = serde_json::Value::from(1);
        }
    }
    value
}

/// Poll `GET {GO}/teams/{team}/members/{user}` until `until` accepts the status and body, or
/// give up after `window`. The sync runs after the response on both servers, so this is the only
/// way to see it.
async fn wait_for_team_member<F>(
    client: &reqwest::Client,
    admin: &str,
    team: &str,
    user: &str,
    window: std::time::Duration,
    until: F,
) -> (u16, serde_json::Value)
where
    F: Fn(u16, &serde_json::Value) -> bool,
{
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let response = client
            .get(format!("{GO}/api/v4/teams/{team}/members/{user}"))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await
            .expect("Go answers");
        let status = response.status().as_u16();
        let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
        if until(status, &body) || tokio::time::Instant::now() >= deadline {
            return (status, body);
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

fn member_shape(body: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "scheme_admin": body["scheme_admin"],
        "scheme_user": body["scheme_user"],
        "scheme_guest": body["scheme_guest"],
        "roles": body["roles"],
        "delete_at": body["delete_at"],
    })
}

/// The whole life of a team link on each side — link, patch, unlink, unlink again, re-link — and
/// the answers agree at every step once the per-side ids and the clock are taken out.
///
/// Pinned along the way: `link` is a **201** and `patch` a 200 with the same body shape; the
/// unlink is `{"status":"OK"}` with no newline; a second unlink is the 400
/// `app.group.group_syncable_already_deleted`; and a re-link **starts from nothing** — the
/// `scheme_admin` the patch set is gone (group.go:385), which the parity of the two bodies pins
/// because the Go side's re-link forgets it too.
#[tokio::test]
async fn licensed_team_link_patch_and_unlink_agree_at_every_step() {
    if !stack_enabled() {
        return;
    }
    let _turn = LICENSED_TURN.lock().await;
    let client = client();
    let fx = licensed_fixture(&client).await;
    let admin = &fx.admin;
    let mut outcomes = Vec::new();
    for side in [&fx.go, &fx.rs] {
        let mut steps = Vec::new();
        let (status, body) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::POST,
            &link(side.group, "teams", &side.team),
            r#"{"auto_add":true}"#,
        )
        .await;
        assert_eq!(
            status,
            201,
            "{}: link: {}",
            side.base,
            String::from_utf8_lossy(&body)
        );
        assert!(!body.ends_with(b"\n"), "json.Marshal + w.Write: no newline");
        steps.push(normalised_syncable(&body, side));

        let (status, body) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::PUT,
            &patch(side.group, "teams", &side.team),
            r#"{"scheme_admin":true}"#,
        )
        .await;
        assert_eq!(
            status,
            200,
            "{}: patch: {}",
            side.base,
            String::from_utf8_lossy(&body)
        );
        steps.push(normalised_syncable(&body, side));

        let (status, body) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::DELETE,
            &link(side.group, "teams", &side.team),
            "",
        )
        .await;
        assert_eq!(
            status,
            200,
            "{}: unlink: {}",
            side.base,
            String::from_utf8_lossy(&body)
        );
        assert_eq!(body, br#"{"status":"OK"}"#, "ReturnStatusOK, no newline");

        let (status, body) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::DELETE,
            &link(side.group, "teams", &side.team),
            "",
        )
        .await;
        assert_eq!(status, 400, "{}: second unlink", side.base);
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["id"], "app.group.group_syncable_already_deleted");

        let (status, body) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::POST,
            &link(side.group, "teams", &side.team),
            r#"{"auto_add":false}"#,
        )
        .await;
        assert_eq!(
            status,
            201,
            "{}: re-link: {}",
            side.base,
            String::from_utf8_lossy(&body)
        );
        let relinked = normalised_syncable(&body, side);
        assert_eq!(
            relinked["scheme_admin"], false,
            "a re-link starts from a zero value"
        );
        assert_eq!(relinked["delete_at"], 0, "and the row is live again");
        steps.push(relinked);

        let (status, _) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::DELETE,
            &link(side.group, "teams", &side.team),
            "",
        )
        .await;
        assert_eq!(status, 200, "{}: cleanup unlink", side.base);
        outcomes.push(steps);
    }
    assert_eq!(
        outcomes[0], outcomes[1],
        "the Go side and the Rust side disagree"
    );
    assert_eq!(outcomes[0][1]["scheme_admin"], true, "the patch took");
}

/// The sync behind the response: a member of the group who is in no fixture team is in the
/// team after the link, is a scheme admin after `scheme_admin` is patched on, and is gone after
/// the unlink once the team is group-constrained. Both syncs write the one database, so each side
/// has its own group, team and member, and the two members' rows are compared as shapes.
#[tokio::test]
async fn licensed_link_syncs_memberships_and_roles_after_the_response() {
    if !stack_enabled() {
        return;
    }
    let _turn = LICENSED_TURN.lock().await;
    let client = client();
    let fx = licensed_fixture(&client).await;
    let admin = &fx.admin;
    let pool = pool().await;
    let window = std::time::Duration::from_secs(15);
    let mut outcomes = Vec::new();
    for side in [&fx.go, &fx.rs] {
        let team = &side.constrained_team;
        let (status, _) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::POST,
            &link(side.group, "teams", team),
            r#"{"auto_add":true}"#,
        )
        .await;
        assert_eq!(status, 201, "{}: link", side.base);
        let (status, joined) = wait_for_team_member(
            &client,
            admin,
            team,
            &side.member.id,
            window,
            |status, body| status == 200 && body["delete_at"] == 0,
        )
        .await;
        assert_eq!(
            status, 200,
            "{}: the member was not added to the team by the sync: {joined}",
            side.base
        );

        let (status, _) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::PUT,
            &patch(side.group, "teams", team),
            r#"{"scheme_admin":true}"#,
        )
        .await;
        assert_eq!(status, 200, "{}: patch", side.base);
        let (_, promoted) = wait_for_team_member(
            &client,
            admin,
            team,
            &side.member.id,
            window,
            |status, body| status == 200 && body["scheme_admin"] == true,
        )
        .await;
        assert_eq!(
            promoted["scheme_admin"], true,
            "{}: the role sync did not promote the member: {promoted}",
            side.base
        );

        // Only a group-constrained team sheds members on unlink (`TeamMembersToRemove`). The
        // oracle caches teams, so it is told; this side reads the row.
        sqlx::query("UPDATE teams SET groupconstrained = true WHERE id = $1")
            .bind(team)
            .execute(&pool)
            .await
            .expect("the team is constrained");
        let _ = client
            .post(format!("{}/api/v4/caches/invalidate", fx.pair.go))
            .header("Authorization", format!("Bearer {admin}"))
            .send()
            .await;
        let (status, _) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::DELETE,
            &link(side.group, "teams", team),
            "",
        )
        .await;
        // Unconstrain first, so a failure below cannot leave the team constrained for the
        // other tests (it did once, and the symptom was `group_not_associated_to_synced_team`
        // on a channel link two tests later).
        let unlinked = status;
        let removed = wait_for_team_member(
            &client,
            admin,
            team,
            &side.member.id,
            window,
            // `RemoveUserFromTeam` soft-deletes the membership, and Go's `GetTeamMember`
            // returns a soft-deleted row — so "gone" is `delete_at` set, not a 404.
            |status, body| status == 404 || body["delete_at"].as_i64().unwrap_or(0) > 0,
        )
        .await;
        sqlx::query("UPDATE teams SET groupconstrained = false WHERE id = $1")
            .bind(team)
            .execute(&pool)
            .await
            .expect("the team is unconstrained again");
        assert_eq!(unlinked, 200, "{}: unlink", side.base);
        let (status, gone) = removed;
        assert!(
            status == 404 || gone["delete_at"].as_i64().unwrap_or(0) > 0,
            "{}: the member was not removed by the unlink: {gone}",
            side.base
        );
        outcomes.push((member_shape(&joined), member_shape(&promoted), status));
    }
    assert_eq!(
        outcomes[0], outcomes[1],
        "the synced memberships differ between the sides"
    );
}

/// Linking a **channel** links its team too, and unlinking the team takes the channel link with
/// it. Read back through the oracle's own list routes, on both sides' fixtures.
#[tokio::test]
async fn licensed_channel_link_links_the_team_and_a_team_unlink_cascades() {
    if !stack_enabled() {
        return;
    }
    let _turn = LICENSED_TURN.lock().await;
    let client = client();
    let fx = licensed_fixture(&client).await;
    let admin = &fx.admin;
    let mut outcomes = Vec::new();
    for side in [&fx.go, &fx.rs] {
        let (status, body) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::POST,
            &link(side.group, "channels", &side.channel),
            r#"{"auto_add":true}"#,
        )
        .await;
        assert_eq!(
            status,
            201,
            "{}: channel link: {}",
            side.base,
            String::from_utf8_lossy(&body)
        );
        let linked = normalised_syncable(&body, side);
        assert_eq!(
            linked["team_id"], "<team>",
            "the channel syncable carries its team"
        );

        let list = async |kind: &str| -> Vec<serde_json::Value> {
            let (status, body, _) = send(
                &client,
                &fx.pair.go,
                admin,
                reqwest::Method::GET,
                &format!("/api/v4/groups/{}/{kind}", side.group),
                "",
            )
            .await;
            assert_eq!(
                status,
                200,
                "listing {kind}: {}",
                String::from_utf8_lossy(&body)
            );
            let items: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
            items
                .into_iter()
                .map(|item| normalised_syncable(&serde_json::to_vec(&item).unwrap(), side))
                .collect()
        };
        let teams = list("teams").await;
        let channels = list("channels").await;
        assert_eq!(
            teams.len(),
            1,
            "{}: the team was linked implicitly: {teams:?}",
            side.base
        );
        assert_eq!(channels.len(), 1, "{}: {channels:?}", side.base);

        let (status, _) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::DELETE,
            &link(side.group, "teams", &side.team),
            "",
        )
        .await;
        assert_eq!(status, 200, "{}: team unlink", side.base);
        let after_teams = list("teams").await;
        let after_channels = list("channels").await;
        assert!(
            after_teams.is_empty() && after_channels.is_empty(),
            "{}: the team unlink cascades to the channel link: {after_teams:?} {after_channels:?}",
            side.base
        );
        // The channel link is soft-deleted, so unlinking it again is the 400, not a 404.
        let (status, body) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::DELETE,
            &link(side.group, "channels", &side.channel),
            "",
        )
        .await;
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            (status, parsed["id"].as_str()),
            (400, Some("app.group.group_syncable_already_deleted")),
            "{}",
            side.base
        );
        outcomes.push((linked, teams, channels));
    }
    assert_eq!(outcomes[0], outcomes[1]);
}

/// Every refusal behind the gate, on both servers of the pair, with Go's id at Go's status —
/// and the one hand-over this side still makes, for a group that hides its members.
#[tokio::test]
async fn licensed_refusals_match_go_and_a_hidden_group_is_handed_over() {
    if !stack_enabled() {
        return;
    }
    let _turn = LICENSED_TURN.lock().await;
    let client = client();
    let fx = licensed_fixture(&client).await;
    let admin = &fx.admin;
    let team = &fx.rs.team;
    let _channel = &fx.rs.channel;
    let cases: Vec<(&str, reqwest::Method, String, &str, &str, u16, &str)> = vec![
        // (label, method, path, body, token, status, id)
        (
            "custom group is not syncable",
            reqwest::Method::POST,
            link(CUSTOM, "teams", team),
            r#"{"auto_add":true}"#,
            admin,
            400,
            "app.group.crud_permission",
        ),
        (
            "custom group: unlink",
            reqwest::Method::DELETE,
            link(CUSTOM, "teams", team),
            "",
            admin,
            400,
            "app.group.crud_permission",
        ),
        (
            "no such group",
            reqwest::Method::POST,
            link(NOWHERE, "teams", team),
            r#"{"auto_add":true}"#,
            admin,
            404,
            "app.group.no_rows",
        ),
        (
            "no such team",
            reqwest::Method::POST,
            link(SYNCABLE_RS, "teams", NOWHERE),
            r#"{"auto_add":true}"#,
            admin,
            404,
            "store.sql_channel.get.existing.app_error",
        ),
        (
            "no such channel",
            reqwest::Method::POST,
            link(SYNCABLE_RS, "channels", NOWHERE),
            r#"{"auto_add":true}"#,
            admin,
            404,
            "app.channel.get.existing.app_error",
        ),
        (
            "malformed link body",
            reqwest::Method::POST,
            link(SYNCABLE_RS, "teams", team),
            "{",
            admin,
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "null link body",
            reqwest::Method::POST,
            link(SYNCABLE_RS, "teams", team),
            "null",
            admin,
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "malformed patch body",
            reqwest::Method::PUT,
            patch(SYNCABLE_RS, "teams", team),
            "[1]",
            admin,
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "patch of a never-linked team",
            reqwest::Method::PUT,
            patch(SYNCABLE_RS, "teams", &fx.never_team),
            r#"{"auto_add":true}"#,
            admin,
            404,
            "app.group.no_rows",
        ),
        (
            "unlink of a never-linked channel",
            reqwest::Method::DELETE,
            link(SYNCABLE_RS, "channels", &fx.never_channel),
            "",
            admin,
            404,
            "app.group.no_rows",
        ),
        (
            "group id fails IsValidId before the syncable id",
            reqwest::Method::POST,
            link("abc", "teams", "0"),
            r#"{}"#,
            admin,
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "outsider links a team",
            reqwest::Method::POST,
            link(SYNCABLE_RS, "teams", team),
            r#"{"auto_add":true}"#,
            &fx.outsider.token,
            403,
            "api.context.permissions.app_error",
        ),
        (
            "outsider unlinks a team",
            reqwest::Method::DELETE,
            link(SYNCABLE_RS, "teams", team),
            "",
            &fx.outsider.token,
            403,
            "api.context.permissions.app_error",
        ),
        // The channel arm asks `SessionHasPermissionToTeam` with the **channel's** id
        // (group.go:706), so a team admin's team roles are never found and the question falls
        // to their system roles — which a plain user's do not answer.
        // The channel is in the never-linked team, so the team question is reached whatever
        // state the other tests left `team`'s link in — a soft-deleted link still counts as
        // "synced" there, since `GetGroupSyncable` has no `DeleteAt` filter.
        (
            "team admin links a channel of an unsynced team",
            reqwest::Method::POST,
            link(SYNCABLE_RS, "channels", &fx.never_channel),
            r#"{"auto_add":true}"#,
            &fx.team_admin.token,
            403,
            "api.context.permissions.app_error",
        ),
        // `scheme_admin` in the patch asks for `manage_team_roles`, which a team admin holds.
        // But the link precedes it and the outsider fails there first: same 403, and the id
        // cannot tell the two apart — the *body* of the permission error names the permission
        // in `detailed_error`, which is wiped, so both really are one wire shape.
        (
            "outsider sets scheme_admin",
            reqwest::Method::POST,
            link(SYNCABLE_RS, "teams", team),
            r#"{"scheme_admin":true}"#,
            &fx.outsider.token,
            403,
            "api.context.permissions.app_error",
        ),
    ];
    for (label, method, path, body, token, status, id) in cases {
        let (go_status, go, _) =
            send(&client, &fx.pair.go, token, method.clone(), &path, body).await;
        let (rs_status, rs, served_by) =
            send(&client, &fx.pair.rust, token, method.clone(), &path, body).await;
        assert_eq!(served_by.as_deref(), Some("rust"), "{label}: forwarded");
        assert_eq!(
            go_status,
            status,
            "{label}: Go: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(
            rs_status,
            go_status,
            "{label}: {}",
            String::from_utf8_lossy(&rs)
        );
        let parsed = assert_error_bodies_match_except_known_gaps(&go, &rs, label);
        assert_eq!(parsed["id"], id, "{label}");
        assert!(
            !rs.ends_with(b"\n"),
            "{label}: error bodies carry no newline"
        );
    }

    // The hand-over: `!group.AllowReference` asks `SessionHasPermissionToGroup`, which is not
    // ported here, so the request goes to Go whole — before any write — and Go's own answer
    // comes back. See [D-531].
    for (method, path, body) in [
        (
            reqwest::Method::POST,
            link(HIDDEN, "teams", team),
            r#"{"auto_add":true}"#,
        ),
        (
            reqwest::Method::PUT,
            patch(HIDDEN, "teams", team),
            r#"{"auto_add":true}"#,
        ),
        (reqwest::Method::DELETE, link(HIDDEN, "teams", team), ""),
    ] {
        let (go_status, go, _) = send(
            &client,
            &fx.pair.go,
            &fx.outsider.token,
            method.clone(),
            &path,
            body,
        )
        .await;
        let (rs_status, rs, served_by) = send(
            &client,
            &fx.pair.rust,
            &fx.outsider.token,
            method.clone(),
            &path,
            body,
        )
        .await;
        assert_eq!(
            served_by.as_deref(),
            Some("go"),
            "{method} {path}: a hidden group is handed over"
        );
        assert_eq!(rs_status, go_status, "{method} {path}");
        assert_eq!(
            go_status, 403,
            "{method} {path}: an outsider lacks sysconsole_read_user_management_groups"
        );
        assert_forwarded_bodies_match(&go, &rs, &format!("{method} {path}"));
    }
}

/// The websocket half of a link and an unlink: `received_group_associated_to_team` with the
/// team as broadcast and the group in `data`, then `..._not_associated_to_team`. One probe per
/// server, on that server's own fixture.
#[tokio::test]
async fn licensed_link_and_unlink_publish_the_group_association_events() {
    if !stack_enabled() {
        return;
    }
    let _stream = common::BROADCAST_STREAM.lock().await;
    let _turn = LICENSED_TURN.lock().await;
    let client = client();
    let fx = licensed_fixture(&client).await;
    let admin = &fx.admin;
    let mut outcomes = Vec::new();
    for side in [&fx.go, &fx.rs] {
        let mut probe = SocketProbe::connect(&side.base, admin).await;
        let (status, _) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::POST,
            &link(side.group, "teams", &side.team),
            r#"{"auto_add":true}"#,
        )
        .await;
        assert_eq!(status, 201, "{}: link", side.base);
        let team = side.team.clone();
        assert!(
            probe
                .collect_until(std::time::Duration::from_secs(5), |frames| frames
                    .iter()
                    .any(|f| f["event"] == "received_group_associated_to_team"
                        && f["broadcast"]["team_id"] == team.as_str()))
                .await,
            "{}: no received_group_associated_to_team for the team: {:?}",
            side.base,
            probe.frames()
        );
        let (status, _) = licensed_send(
            &client,
            fx,
            &side.base,
            admin,
            reqwest::Method::DELETE,
            &link(side.group, "teams", &side.team),
            "",
        )
        .await;
        assert_eq!(status, 200, "{}: unlink", side.base);
        assert!(
            probe
                .collect_until(std::time::Duration::from_secs(5), |frames| frames
                    .iter()
                    .any(|f| f["event"] == "received_group_not_associated_to_team"
                        && f["broadcast"]["team_id"] == team.as_str()))
                .await,
            "{}: no received_group_not_associated_to_team: {:?}",
            side.base,
            probe.frames()
        );
        let shape = |name: &str| -> serde_json::Value {
            let frame = probe
                .events_named(name)
                .into_iter()
                .find(|f| f["broadcast"]["team_id"] == team.as_str())
                .expect("collected above");
            let mut frame = frame;
            frame.as_object_mut().unwrap().remove("seq");
            frame["broadcast"]["team_id"] = serde_json::Value::String("<team>".into());
            frame["data"]["group_id"] =
                serde_json::Value::String(if frame["data"]["group_id"] == side.group {
                    "<group>".into()
                } else {
                    frame["data"]["group_id"].to_string()
                });
            frame
        };
        outcomes.push((
            shape("received_group_associated_to_team"),
            shape("received_group_not_associated_to_team"),
        ));
    }
    assert_eq!(outcomes[0], outcomes[1], "the two servers' frames differ");
}
