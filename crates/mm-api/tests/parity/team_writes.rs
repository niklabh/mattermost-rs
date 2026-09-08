//! Cross-server parity for four team writes: `updateTeam`, `patchTeam`, `restoreTeam` and
//! `regenerateTeamInviteId`.
//!
//! Two things make this group different from the CRUD ones before it. The **second permission is
//! conditional** — `invite_user` is required only when certain fields change, and the update and
//! patch routes decide that differently — and the **websocket event carries less than the HTTP
//! response**, because the two use different sanitisers.
//!
//! Reads back through the server that wrote, per [D-190].
//!
//! ```sh
//! docker compose up -d && scripts/parity.sh --test parity team_writes
//! ```

use std::time::Duration;

use crate::common;

use common::{GO, RUST, SocketProbe, client, create_team, go_minted_token, stack_enabled};

async fn send(
    http: &reqwest::Client,
    method: reqwest::Method,
    base: &str,
    token: &str,
    path: &str,
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
        .unwrap_or_else(|e| panic!("{base}{path} unreachable: {e}"));
    let status = response.status().as_u16();
    if base == RUST {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.text().await.expect("a body"))
}

async fn team(http: &reqwest::Client, base: &str, token: &str, id: &str) -> serde_json::Value {
    let (_status, raw) = send(
        http,
        reqwest::Method::GET,
        base,
        token,
        &format!("/api/v4/teams/{id}"),
        None,
    )
    .await;
    serde_json::from_str(&raw).expect("a team")
}

/// Everything but the values that cannot match between two independently created teams.
fn normalise(value: &serde_json::Value) -> serde_json::Value {
    let mut out = value.clone();
    let Some(object) = out.as_object_mut() else {
        return out;
    };
    for key in ["id", "name", "display_name", "invite_id", "email"] {
        if let Some(v) = object.get(key) {
            let present = v.as_str().is_some_and(|s| !s.is_empty());
            object.insert(key.to_owned(), serde_json::json!(present));
        }
    }
    for key in ["create_at", "update_at"] {
        if let Some(v) = object.get(key) {
            let nonzero = v.as_i64().unwrap_or(0) > 0;
            object.insert(key.to_owned(), serde_json::json!(nonzero));
        }
    }
    out
}

#[tokio::test]
async fn updating_a_team_writes_seven_fields_and_discards_the_rest() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let go_team = create_team(&http, &token, "twupdg").await;
    let rust_team = create_team(&http, &token, "twupdr").await;

    // **Distinct display names per server.** `teams_all`'s fixture refuses to build when two
    // teams share one, because its `ORDER BY DisplayName` has no tiebreak — and that panic leaves
    // its `OnceCell` half-initialised, so the *next* test retries the build and collides on the
    // team name. Two words in this fixture took out ten tests in another suite.
    let body = |id: &str, display_name: &str, stored: &serde_json::Value| {
        serde_json::json!({
            "id": id,
            "display_name": display_name,
            "description": "mmrs parity update",
            "company_name": "mmrs co",
            // Round-tripped so the conditional `invite_user` check does not fire.
            "allow_open_invite": stored["allow_open_invite"],
            "allowed_domains": stored["allowed_domains"],
            // **Discarded by the app layer**, whatever we send.
            "email": "MMRS-Not-Applied@example.invalid",
            "type": "P",
            "invite_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
            "delete_at": 12345,
        })
    };

    let go_stored = team(&http, GO, &token, &go_team).await;
    let rust_stored = team(&http, RUST, &token, &rust_team).await;

    let (go_status, go_raw) = send(
        &http,
        reqwest::Method::PUT,
        GO,
        &token,
        &format!("/api/v4/teams/{go_team}"),
        Some(&body(&go_team, "mmrs renamed go", &go_stored)),
    )
    .await;
    let (rust_status, rust_raw) = send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &token,
        &format!("/api/v4/teams/{rust_team}"),
        Some(&body(&rust_team, "mmrs renamed rust", &rust_stored)),
    )
    .await;

    assert_eq!(go_status, 200, "Go answers OK: {go_raw}");
    assert_eq!(rust_status, go_status, "the update status differs");
    assert!(
        go_raw.ends_with('\n'),
        "Go's update body is encoder-framed: {go_raw:?}"
    );
    assert_eq!(rust_raw.ends_with('\n'), go_raw.ends_with('\n'));

    let go_updated: serde_json::Value = serde_json::from_str(&go_raw).expect("a team");
    let rust_updated: serde_json::Value = serde_json::from_str(&rust_raw).expect("a team");
    assert_eq!(
        normalise(&go_updated),
        normalise(&rust_updated),
        "the updated team differs:\n go: {go_updated}\nrust: {rust_updated}"
    );

    // **The seven fields that are written.**
    assert_eq!(rust_updated["display_name"], "mmrs renamed rust");
    assert_eq!(rust_updated["description"], "mmrs parity update");
    assert_eq!(rust_updated["company_name"], "mmrs co");

    // **And the ones that are not.** `Sanitized: true` copies seven fields onto the *stored*
    // team, so a client cannot change a team's type, un-archive it, or choose its invite id
    // through this route — the fields simply do not reach the write.
    for (field, sent) in [
        ("type", serde_json::json!("P")),
        ("delete_at", serde_json::json!(12345)),
    ] {
        assert_ne!(
            rust_updated[field], sent,
            "{field} must be discarded, not written"
        );
        assert_eq!(
            rust_updated[field], rust_stored[field],
            "{field} must keep its stored value"
        );
    }
    assert_eq!(
        rust_updated["invite_id"], rust_stored["invite_id"],
        "invite_id is discarded too"
    );

    // The row, read back through the writing server.
    let reread = team(&http, RUST, &token, &rust_team).await;
    assert_eq!(reread["display_name"], "mmrs renamed rust");
    assert_eq!(
        reread["type"], rust_stored["type"],
        "the stored type is unchanged"
    );
}

#[tokio::test]
async fn closing_open_invitations_regenerates_the_invite_id() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    // A side effect nothing in the request names: `patch.AllowOpenInvite != nil &&
    // !*patch.AllowOpenInvite` mints a new invite id, invalidating every link already handed out.
    for base in [GO, RUST] {
        let id = create_team(&http, &token, if base == GO { "twinvg" } else { "twinvr" }).await;
        let before = team(&http, base, &token, &id).await;

        // Open it first, so the close is a real transition.
        send(
            &http,
            reqwest::Method::PUT,
            base,
            &token,
            &format!("/api/v4/teams/{id}/patch"),
            Some(&serde_json::json!({"allow_open_invite": true})),
        )
        .await;
        let opened = team(&http, base, &token, &id).await;

        let (status, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &token,
            &format!("/api/v4/teams/{id}/patch"),
            Some(&serde_json::json!({"allow_open_invite": false})),
        )
        .await;
        assert_eq!(status, 200, "{base} patched the team: {raw}");
        let closed: serde_json::Value = serde_json::from_str(&raw).expect("a team");

        assert_eq!(closed["allow_open_invite"], false);
        assert_ne!(
            closed["invite_id"], opened["invite_id"],
            "{base}: closing open invitations must mint a new invite id"
        );
        assert_ne!(
            closed["invite_id"], before["invite_id"],
            "{base}: and it is not the original either"
        );

        // Opening it again must **not** regenerate — the branch is on the value, not on change.
        let (_status, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &token,
            &format!("/api/v4/teams/{id}/patch"),
            Some(&serde_json::json!({"allow_open_invite": true})),
        )
        .await;
        let reopened: serde_json::Value = serde_json::from_str(&raw).expect("a team");
        assert_eq!(
            reopened["invite_id"], closed["invite_id"],
            "{base}: opening must leave the invite id alone"
        );
    }
}

#[tokio::test]
async fn a_patch_changes_only_the_fields_it_names() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let go_team = create_team(&http, &token, "twpatg").await;
    let rust_team = create_team(&http, &token, "twpatr").await;

    let patch = serde_json::json!({"description": "mmrs patched description"});

    let mut answers = Vec::new();
    for (base, id) in [(GO, &go_team), (RUST, &rust_team)] {
        let before = team(&http, base, &token, id).await;
        let (status, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &token,
            &format!("/api/v4/teams/{id}/patch"),
            Some(&patch),
        )
        .await;
        assert_eq!(status, 200, "{base} patched: {raw}");
        let after: serde_json::Value = serde_json::from_str(&raw).expect("a team");
        assert_eq!(after["description"], "mmrs patched description");
        assert_eq!(
            after["display_name"], before["display_name"],
            "{base} changed a field the patch did not name"
        );
        assert_eq!(
            after["invite_id"], before["invite_id"],
            "{base} regenerated the invite id for a patch that did not touch open invitations"
        );
        assert!(
            after["update_at"].as_i64() >= before["update_at"].as_i64(),
            "{base} did not move update_at"
        );
        answers.push((raw, after));
    }

    assert_eq!(
        normalise(&answers[0].1),
        normalise(&answers[1].1),
        "the patched teams differ"
    );
    assert_eq!(
        answers[0].0.ends_with('\n'),
        answers[1].0.ends_with('\n'),
        "the patch body's framing differs"
    );
}

#[tokio::test]
async fn restoring_and_regenerating_agree_and_publish_update_team() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let go_team = create_team(&http, &token, "twresg").await;
    let rust_team = create_team(&http, &token, "twresr").await;

    let mut go_socket = SocketProbe::connect(GO, &token).await;
    let mut rust_socket = SocketProbe::connect(RUST, &token).await;

    // Regenerating publishes **`update_team`**, not an event of its own — a client cannot tell it
    // from an ordinary update on the socket alone.
    let mut answers = Vec::new();
    for (base, id) in [(GO, &go_team), (RUST, &rust_team)] {
        let before = team(&http, base, &token, id).await;
        let (status, raw) = send(
            &http,
            reqwest::Method::POST,
            base,
            &token,
            &format!("/api/v4/teams/{id}/regenerate_invite_id"),
            Some(&serde_json::json!({})),
        )
        .await;
        assert_eq!(status, 200, "{base} regenerated: {raw}");
        let after: serde_json::Value = serde_json::from_str(&raw).expect("a team");
        assert_ne!(
            after["invite_id"], before["invite_id"],
            "{base} did not mint a new invite id"
        );
        assert_eq!(
            after["invite_id"].as_str().map(str::len),
            Some(26),
            "{base}: the invite id is an id"
        );
        answers.push(after);
    }
    assert_eq!(
        normalise(&answers[0]),
        normalise(&answers[1]),
        "the regenerated teams differ"
    );

    go_socket.collect_for(Duration::from_millis(900)).await;
    rust_socket.collect_for(Duration::from_millis(900)).await;

    let go_events = team_events(&go_socket, "update_team", &go_team);
    let rust_events = team_events(&rust_socket, "update_team", &rust_team);
    assert_eq!(
        go_events.len(),
        1,
        "Go published one update_team: {:?}",
        go_socket.raw
    );
    assert_eq!(
        rust_events.len(),
        1,
        "we published one update_team: {:?}",
        rust_socket.raw
    );

    // Addressed to the **team**, so the hub delivers it to every member via the session's team
    // memberships — no channel and no user. The two teams are different rows, so the `team_id`
    // itself cannot match; everything else about the addressing must.
    let addressing = |event: &serde_json::Value| {
        let mut broadcast = event["broadcast"].clone();
        broadcast["team_id"] = serde_json::json!("<team>");
        broadcast
    };
    assert_eq!(
        addressing(&go_events[0]),
        addressing(&rust_events[0]),
        "the team event's addressing differs"
    );
    assert_eq!(rust_events[0]["broadcast"]["team_id"], rust_team);
    assert_eq!(go_events[0]["broadcast"]["team_id"], go_team);
    assert_eq!(rust_events[0]["broadcast"]["channel_id"], "");
    assert_eq!(rust_events[0]["broadcast"]["user_id"], "");

    // **The event carries less than the response.** `sendTeamEvent` runs the *unconditional*
    // sanitiser, which clears `Email` and `InviteId`; the HTTP answer runs the session-aware one,
    // which leaves them for an admin. So the invite id this route exists to produce is **not** on
    // the socket.
    let carried: serde_json::Value = rust_events[0]["data"]["team"]
        .as_str()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .expect("the team is a JSON string");
    assert_eq!(
        carried["invite_id"], "",
        "the event's team is sanitised: no invite id"
    );
    assert_eq!(carried["email"], "", "and no email");
    assert_ne!(
        answers[1]["invite_id"], "",
        "while the HTTP answer does carry it, for an admin"
    );

    // Archive and restore, through the server that will restore it.
    send(
        &http,
        reqwest::Method::DELETE,
        GO,
        &token,
        &format!("/api/v4/teams/{go_team}"),
        None,
    )
    .await;
    send(
        &http,
        reqwest::Method::DELETE,
        GO,
        &token,
        &format!("/api/v4/teams/{rust_team}"),
        None,
    )
    .await;

    let (go_status, go_raw) = send(
        &http,
        reqwest::Method::POST,
        GO,
        &token,
        &format!("/api/v4/teams/{go_team}/restore"),
        Some(&serde_json::json!({})),
    )
    .await;
    let (rust_status, rust_raw) = send(
        &http,
        reqwest::Method::POST,
        RUST,
        &token,
        &format!("/api/v4/teams/{rust_team}/restore"),
        Some(&serde_json::json!({})),
    )
    .await;
    assert_eq!(go_status, 200, "Go restored: {go_raw}");
    assert_eq!(rust_status, go_status, "the restore status differs");

    let go_restored: serde_json::Value = serde_json::from_str(&go_raw).expect("a team");
    let rust_restored: serde_json::Value = serde_json::from_str(&rust_raw).expect("a team");
    assert_eq!(go_restored["delete_at"], 0, "Go cleared delete_at");
    assert_eq!(rust_restored["delete_at"], 0, "we cleared delete_at");
    assert_eq!(
        normalise(&go_restored),
        normalise(&rust_restored),
        "the restored teams differ"
    );

    rust_socket.collect_for(Duration::from_millis(900)).await;
    assert_eq!(
        team_events(&rust_socket, "restore_team", &rust_team).len(),
        1,
        "the restore publishes restore_team, not update_team: {:?}",
        rust_socket.raw
    );
}

/// Team events on `probe` concerning `team_id`. The team is a JSON **string** inside `data`, the
/// same double encoding the reaction and draft events use.
fn team_events(probe: &SocketProbe, event_type: &str, team_id: &str) -> Vec<serde_json::Value> {
    probe
        .events_named(event_type)
        .into_iter()
        .filter(|frame| frame["broadcast"]["team_id"] == team_id)
        .collect()
}

#[tokio::test]
async fn the_id_checks_agree() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let id = create_team(&http, &token, "twids").await;

    // A body whose id disagrees with the path. **The parameter Go names is `id`, not `team_id`**
    // — the body's field rather than the path's segment.
    let (go_status, go_raw) = send(
        &http,
        reqwest::Method::PUT,
        GO,
        &token,
        &format!("/api/v4/teams/{id}"),
        Some(&serde_json::json!({"id": "aaaaaaaaaaaaaaaaaaaaaaaaaa", "display_name": "x"})),
    )
    .await;
    let (rust_status, rust_raw) = send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &token,
        &format!("/api/v4/teams/{id}"),
        Some(&serde_json::json!({"id": "aaaaaaaaaaaaaaaaaaaaaaaaaa", "display_name": "x"})),
    )
    .await;
    assert_eq!(go_status, 400, "Go on an id mismatch: {go_raw}");
    assert_eq!(rust_status, go_status);
    let go_body: serde_json::Value = serde_json::from_str(&go_raw).expect("an AppError");
    let rust_body: serde_json::Value = serde_json::from_str(&rust_raw).expect("an AppError");
    assert_eq!(go_body["id"], rust_body["id"]);
    assert_eq!(
        go_body["params"]["Name"].as_str().unwrap_or("id"),
        rust_body["params"]["Name"].as_str().unwrap_or("id"),
    );

    // A malformed team id in the path, on all four routes.
    for (method, path) in [
        (reqwest::Method::PUT, "/api/v4/teams/short".to_owned()),
        (reqwest::Method::PUT, "/api/v4/teams/short/patch".to_owned()),
        (
            reqwest::Method::POST,
            "/api/v4/teams/short/restore".to_owned(),
        ),
        (
            reqwest::Method::POST,
            "/api/v4/teams/short/regenerate_invite_id".to_owned(),
        ),
    ] {
        let (go_status, go_raw) = send(
            &http,
            method.clone(),
            GO,
            &token,
            &path,
            Some(&serde_json::json!({})),
        )
        .await;
        let (rust_status, rust_raw) = send(
            &http,
            method,
            RUST,
            &token,
            &path,
            Some(&serde_json::json!({})),
        )
        .await;
        assert_eq!(go_status, 400, "Go on {path}: {go_raw}");
        assert_eq!(rust_status, go_status, "the status differs for {path}");
        let go_body: serde_json::Value = serde_json::from_str(&go_raw).expect("an AppError");
        let rust_body: serde_json::Value = serde_json::from_str(&rust_raw).expect("an AppError");
        assert_eq!(go_body["id"], rust_body["id"], "the id differs for {path}");
    }
}

#[tokio::test]
async fn the_name_sentinel_leaves_the_name_alone() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;

    // **`-` means "leave it alone".** The guard is
    // `name != "" && name != oldTeam.Name && name != "-"`, and without the last clause a client
    // sending the sentinel would rename the team to a single hyphen — which `Team::is_valid`
    // rejects, so the request would 400 rather than succeed.
    for base in [GO, RUST] {
        let id = create_team(
            &http,
            &token,
            if base == GO { "twsentg" } else { "twsentr" },
        )
        .await;
        let before = team(&http, base, &token, &id).await;

        let (status, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &token,
            &format!("/api/v4/teams/{id}"),
            Some(&serde_json::json!({
                "id": id,
                "name": "-",
                "display_name": format!("mmrs sentinel {base}"),
                "allow_open_invite": before["allow_open_invite"],
                "allowed_domains": before["allowed_domains"],
            })),
        )
        .await;
        assert_eq!(status, 200, "{base} refused the sentinel: {raw}");
        let after: serde_json::Value = serde_json::from_str(&raw).expect("a team");
        assert_eq!(
            after["name"], before["name"],
            "{base}: `-` must leave the name alone"
        );
        assert_eq!(after["display_name"], format!("mmrs sentinel {base}"));
    }
}

#[tokio::test]
async fn changing_open_invitations_needs_invite_user_and_the_two_routes_differ() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let admin = go_minted_token(&http).await;

    // **`purge_api_fixtures` before `plant_role`**: the purge sweeps `mmrs_role_%`, and
    // `create_plain_user` triggers it — planting first and creating second deletes the role
    // between the two.
    common::purge_api_fixtures().await;

    // **No stock role separates `manage_team` from `invite_user`** — `team_admin` holds both — so
    // a mutation dropping the conditional check is invisible to any fixture built from stock
    // roles. All three of them survived the first run of this plan.
    let Some(role) = common::plant_role("twmanage", "manage_team").await else {
        return; // no DATABASE_URL
    };
    let team_id = create_team(&http, &admin, "twperm").await;
    let plain = common::create_plain_user(&http, &admin, &team_id, "twmanage").await;
    common::set_user_roles(&plain.id, &format!("system_user {role}")).await;
    // **`team_user` holds `invite_user`**, so an ordinary member of the team has it however
    // narrow the system role is — measured: Go allowed the change until this row was flattened.
    // `POST /teams/{id}/members` always writes `team_user`, so the REST API cannot produce a
    // roleless membership and it is planted directly.
    plant_roleless_team_membership(&team_id, &plain.id).await;
    let manager = common::login_plain_user(&http, "twmanage").await;

    let stored = team(&http, RUST, &admin, &team_id).await;

    // An update that changes **nothing** about invitations is allowed: `manage_team` alone.
    let (status, raw) = send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &manager,
        &format!("/api/v4/teams/{team_id}"),
        Some(&serde_json::json!({
            "id": team_id,
            "display_name": "mmrs manage only",
            "allow_open_invite": stored["allow_open_invite"],
            "allowed_domains": stored["allowed_domains"],
        })),
    )
    .await;
    assert_eq!(
        status, 200,
        "manage_team alone must suffice when invitations do not change: {raw}"
    );

    // The same update **changing** `allow_open_invite` is a 403 naming `invite_user`.
    let flipped = !stored["allow_open_invite"].as_bool().unwrap_or(false);
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &manager,
            &format!("/api/v4/teams/{team_id}"),
            Some(&serde_json::json!({
                "id": team_id,
                "display_name": "mmrs manage only",
                "allow_open_invite": flipped,
                "allowed_domains": stored["allowed_domains"],
            })),
        )
        .await;
        assert_eq!(status, 403, "{base} on a changed allow_open_invite: {raw}");
    }

    // **The patch route decides by presence, not by change.** Naming the field with the value it
    // already has is a 403 here and a 200 on the update route above — the sharpest difference
    // between the two, and the only fixture that can see it.
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::PUT,
            base,
            &manager,
            &format!("/api/v4/teams/{team_id}/patch"),
            Some(&serde_json::json!({
                "allow_open_invite": stored["allow_open_invite"],
            })),
        )
        .await;
        assert_eq!(
            status, 403,
            "{base}: naming allow_open_invite at all needs invite_user: {raw}"
        );
    }

    // A patch naming neither field is allowed.
    let (status, raw) = send(
        &http,
        reqwest::Method::PUT,
        RUST,
        &manager,
        &format!("/api/v4/teams/{team_id}/patch"),
        Some(&serde_json::json!({"description": "mmrs manage only patch"})),
    )
    .await;
    assert_eq!(
        status, 200,
        "a patch that names neither field is allowed: {raw}"
    );

    // And `regenerate_invite_id` requires `invite_user` **unconditionally** — the only team write
    // that does.
    for base in [GO, RUST] {
        let (status, raw) = send(
            &http,
            reqwest::Method::POST,
            base,
            &manager,
            &format!("/api/v4/teams/{team_id}/regenerate_invite_id"),
            Some(&serde_json::json!({})),
        )
        .await;
        assert_eq!(
            status, 403,
            "{base}: regenerating always needs invite_user: {raw}"
        );
    }

    common::delete_plain_user(&http, &admin, &plain.id).await;
}

/// Make `user_id` a member of `team_id` with no roles whatsoever.
///
/// See the note at its call site: every stock team role grants `invite_user`, so this is the only
/// way to hold `manage_team` without it.
async fn plant_roleless_team_membership(team_id: &str, user_id: &str) {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let Ok(pool) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
    else {
        return;
    };
    sqlx::query(
        "UPDATE teammembers SET roles = '', schemeuser = false, schemeadmin = false, \
             schemeguest = false WHERE teamid = $1 AND userid = $2",
    )
    .bind(team_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("the membership is flattened");
}
