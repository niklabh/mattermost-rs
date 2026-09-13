//! Cross-server parity for the **licensed** half of the seven CRUD and membership writes in
//! `api4/group.go`, against the licensed pair (`common::licensed`).
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity group_writes_licensed
//! ```
//!
//! `parity::group_writes` measures the unlicensed contract — one 501 before the body is read.
//! This file is everything behind that gate: the source rule, `licensedAndConfiguredForGroupBySource`,
//! the custom-group permission model (membership grants `custom_group_user`), `patchGroup`'s name
//! derivation and its three collision checks, the two soft-delete mirrors, the two membership
//! writes, and the three websocket events. Every case is put to both licensed servers and
//! compared; nothing here is read off the source and asserted on its own.
//!
//! # Writes need two fixtures, not one
//!
//! A write made through one server is visible to the other — they share a database — so a case
//! that *creates* runs once per server on differently named groups and the two answers are
//! compared after masking what must differ: the id, the name, the timestamps. A case that
//! *reads* (`names`) uses one fixture and expects identical bytes.
//!
//! # Fixtures
//!
//! Groups are created through the licensed **Go** server, so a fixture is Go's own row, and
//! every one carries the `mmrslicgrp` prefix so [`purge`] can sweep them by name, members and
//! all. An `ldap`-source group cannot be created over the API on this stack (no LDAP) and is
//! planted directly, the way `team_admin` plants its custom ones.

use std::time::Duration;

use crate::common;
use common::{
    GO, LicensedPair, SocketProbe, a_team_and_channel_the_user_is_in, client, create_plain_user,
    delete_plain_user, go_minted_token, licensed, logged_in_user_id, stack_enabled,
};

const PREFIX: &str = "mmrslicgrp";

/// **One test at a time.** Every test here sweeps the `mmrslicgrp` prefix on entry and exit, so
/// two running together delete each other's fixtures mid-request — measured as a 404 on a delete
/// of a group the test had just created. Serialised rather than given per-test prefixes, because
/// the sweep is also what makes an aborted run recoverable.
static SUITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A user with **no system roles at all**, logged in after the change so the session carries
/// none either.
///
/// `system_user` holds every custom-group permission in the stock role set — measured: a plain
/// user not in a group patched it and added members to it, 200 both — so a plain user cannot
/// show the permission refusals or the membership grant. A user whose roles are `""` can:
/// as a non-member every custom-group write is a 403, and as a member the implicit
/// `custom_group_user` role is the only thing that lets them through.
async fn roleless_user(
    client: &reqwest::Client,
    admin: &str,
    team: &str,
    tag: &str,
) -> common::PlainUser {
    let user = create_plain_user(client, admin, team, tag).await;
    let pool = pool().await;
    sqlx::query("UPDATE users SET roles = '' WHERE id = $1")
        .bind(&user.id)
        .execute(&pool)
        .await
        .expect("roles cleared");
    // The login below reads the user through Go's cache, which still holds `system_user`.
    common::invalidate_go_caches(client, admin).await;
    let username = client
        .get(format!("{GO}/api/v4/users/{}", user.id))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("user")
        .json::<serde_json::Value>()
        .await
        .expect("json")["username"]
        .as_str()
        .unwrap()
        .to_owned();
    let response = client
        .post(format!("{GO}/api/v4/users/login"))
        .json(&serde_json::json!({ "login_id": username, "password": common::PLAIN_USER_PASSWORD }))
        .send()
        .await
        .expect("login");
    assert_eq!(response.status(), 200, "the roleless user logs in");
    let token = response
        .headers()
        .get("token")
        .and_then(|v| v.to_str().ok())
        .expect("a token")
        .to_owned();
    let me: serde_json::Value = response.json().await.expect("the user");
    assert_eq!(
        me["roles"], "",
        "the session was minted after the roles were cleared"
    );
    common::PlainUser { id: user.id, token }
}
/// A 26-character id that names nothing.
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// One request to one server: `(status, body, x-mmrs-served-by)`.
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

/// The same request to both licensed servers. The Rust side must have served it itself.
async fn both(
    client: &reqwest::Client,
    pair: &LicensedPair,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let (go_status, go, _) = send(client, &pair.go, token, method.clone(), path, body).await;
    let (rs_status, rs, served_by) = send(client, &pair.rust, token, method, path, body).await;
    assert_eq!(
        served_by.as_deref(),
        Some("rust"),
        "{path} was forwarded, so this comparison proves nothing about the Rust handler"
    );
    ((go_status, go), (rs_status, rs))
}

/// Assert two **error** bodies agree in everything but `request_id` and `message` — the same
/// tolerance `common::assert_error_bodies_match_except_known_gaps` gives, returning the id.
fn same_error(go: &[u8], rs: &[u8], context: &str) -> String {
    let parsed = common::assert_error_bodies_match_except_known_gaps(go, rs, context);
    parsed["id"].as_str().unwrap_or_default().to_owned()
}

/// A group body with the per-write fields masked, so two groups created on two servers can be
/// compared: `id`, `name`, `display_name`, `create_at`, `update_at`, and a non-zero `delete_at`.
fn masked_group(value: &serde_json::Value) -> serde_json::Value {
    let mut v = value.clone();
    if let Some(map) = v.as_object_mut() {
        for key in ["id", "name", "display_name", "create_at", "update_at"] {
            if map.contains_key(key) {
                map.insert(key.to_owned(), serde_json::Value::String("<masked>".into()));
            }
        }
        if map.get("delete_at").and_then(|d| d.as_i64()).unwrap_or(0) != 0 {
            map.insert(
                "delete_at".to_owned(),
                serde_json::Value::String("<masked>".into()),
            );
        }
    }
    v
}

/// A group-member body with `create_at`/`delete_at` masked (they are clock readings).
fn masked_members(value: &serde_json::Value) -> serde_json::Value {
    let mut v = value.clone();
    if let Some(list) = v.as_array_mut() {
        for m in list.iter_mut() {
            if let Some(map) = m.as_object_mut() {
                for key in ["create_at", "delete_at"] {
                    if map.get(key).and_then(|d| d.as_i64()).unwrap_or(0) != 0 {
                        map.insert(key.to_owned(), serde_json::Value::String("<masked>".into()));
                    }
                }
            }
        }
    }
    v
}

fn parse(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("not JSON: {e}: {}", String::from_utf8_lossy(bytes)))
}

/// The `member_count` a server reports for a group, read off an empty patch.
async fn member_count(client: &reqwest::Client, base: &str, admin: &str, id: &str) -> i64 {
    let p = format!("/api/v4/groups/{id}/patch");
    let (status, bytes, _) = send(client, base, admin, reqwest::Method::PUT, &p, "{}").await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    parse(&bytes)["member_count"].as_i64().unwrap()
}

async fn pool() -> sqlx::PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is set under parity.sh");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable")
}

/// Remove every group this suite made, members first. Runs at the start of every test (an
/// aborted run leaves rows) and at the end.
async fn purge() {
    let pool = pool().await;
    sqlx::query(
        "DELETE FROM groupmembers WHERE groupid IN (SELECT id FROM usergroups WHERE name LIKE $1)",
    )
    .bind(format!("{PREFIX}%"))
    .execute(&pool)
    .await
    .expect("members purged");
    sqlx::query("DELETE FROM usergroups WHERE name LIKE $1")
        .bind(format!("{PREFIX}%"))
        .execute(&pool)
        .await
        .expect("groups purged");
    // The stock `custom_group_user` role has no permissions; an aborted run of the patch test
    // could leave one on it.
    sqlx::query("UPDATE roles SET permissions = '' WHERE name = 'custom_group_user'")
        .execute(&pool)
        .await
        .expect("the role is stock again");
}

/// Give the implicit `custom_group_user` role a permission — through the licensed Go server's
/// own role API, because that is the only write that also refreshes its role cache
/// (`POST /caches/invalidate` was measured to leave `GET /roles/name/…` stale), **and then
/// through the stack's main Go server with the same body**, for the same reason in the other
/// direction: the main server caches roles for thirty minutes and refreshes only on its own
/// save, so a patch made here alone left its copy of this role stale for the rest of the run
/// and the `roles` suite compared that stale copy against the row (measured 2026-09-13: one
/// `update_at` apart). The main server's save is last, so its cache holds the row every
/// unlicensed comparison reads.
///
/// **The stock role is empty** (`model.MakeDefaultRoles`, role.go:926): being a member of a
/// custom group grants nothing until an administrator edits this role. So the membership branch
/// of `SessionHasPermissionToGroup` is inert on a fresh installation, and reaching it means
/// changing the role — restored the same way afterwards, with [`purge`]'s row reset as the
/// backstop for an aborted run.
async fn set_custom_group_user_permissions(
    client: &reqwest::Client,
    pair: &LicensedPair,
    admin: &str,
    permissions: &[&str],
) {
    let role: serde_json::Value = client
        .get(format!("{}/api/v4/roles/name/custom_group_user", pair.go))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("the role")
        .json()
        .await
        .expect("json");
    let response = client
        .put(format!(
            "{}/api/v4/roles/{}/patch",
            pair.go,
            role["id"].as_str().expect("an id")
        ))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "permissions": permissions }))
        .send()
        .await
        .expect("the role is patched");
    assert_eq!(
        response.status(),
        200,
        "patching custom_group_user: {}",
        response.text().await.unwrap_or_default()
    );
    // **Twice, and the first one is a change.** `patchRole` reads the row from the database,
    // not from the cache, and a patch that changes nothing is not saved — and the licensed
    // server has just written exactly these permissions. Measured 2026-09-13: one patch here
    // with the target set left the main server's cached copy at the pre-run `update_at` for
    // the whole run. So the first patch flips the set (a real change, so it saves and the
    // cache is refreshed), and the second sets the target the same way.
    let toggled: &[&str] = if permissions.is_empty() {
        &["edit_custom_group"]
    } else {
        &[]
    };
    for set in [toggled, permissions] {
        let response = client
            .put(format!(
                "{}/api/v4/roles/{}/patch",
                GO,
                role["id"].as_str().expect("an id")
            ))
            .header("Authorization", format!("Bearer {admin}"))
            .json(&serde_json::json!({ "permissions": set }))
            .send()
            .await
            .expect("the role is patched on the main server too");
        assert_eq!(
            response.status(),
            200,
            "patching custom_group_user on the main server"
        );
    }
}

/// Create a custom group through the licensed Go server, returning its body.
async fn create_via_go(
    client: &reqwest::Client,
    pair: &LicensedPair,
    token: &str,
    tag: &str,
    user_ids: &[&str],
) -> serde_json::Value {
    let body = serde_json::json!({
        "name": format!("{PREFIX}-{tag}"),
        "display_name": format!("Display {tag}"),
        "source": "custom",
        "allow_reference": true,
        "user_ids": user_ids,
    });
    let (status, bytes, _) = send(
        client,
        &pair.go,
        token,
        reqwest::Method::POST,
        "/api/v4/groups",
        &body.to_string(),
    )
    .await;
    assert_eq!(
        status,
        201,
        "fixture group {tag}: {}",
        String::from_utf8_lossy(&bytes)
    );
    parse(&bytes)
}

/// Plant a group of another source straight into `UserGroups`: no LDAP is configured on this
/// stack, so an `ldap` group cannot be made over the API, and every non-custom refusal needs one.
async fn plant_group(id: &str, tag: &str, source: &str, allow_reference: bool) {
    let pool = pool().await;
    sqlx::query(
        "INSERT INTO usergroups
           (id, name, displayname, description, source, remoteid, createat, updateat, deleteat,
            allowreference)
         VALUES ($1, $2, $3, 'planted by group_writes_licensed', $4, $5, 1700000000000,
                 1700000000000, 0, $6)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(format!("{PREFIX}-{tag}"))
    .bind(format!("Display {tag}"))
    .bind(source)
    .bind(if source == "custom" {
        None
    } else {
        Some("planted-remote-id")
    })
    .bind(allow_reference)
    .execute(&pool)
    .await
    .expect("the group is planted");
}

const LDAP_GROUP: &str = "mmrslicgrpldap000000000001";
const HIDDEN_GROUP: &str = "mmrslicgrphidden0000000001";

/// Group ids are 26 characters, and a 25-character literal reads as one — asserted at compile time.
const _: () = assert!(LDAP_GROUP.len() == 26 && HIDDEN_GROUP.len() == 26);

// ---------------------------------------------------------------------------------------------
// createGroup
// ---------------------------------------------------------------------------------------------

/// A 201 — not a 200 — with the created group, `member_count` included and `member_ids` null,
/// and no trailing newline. Two fixtures, one per server, compared masked.
#[tokio::test]
async fn create_answers_201_with_the_member_count() {
    if !stack_enabled() {
        return;
    }
    let _suite = SUITE.lock().await;
    purge().await;
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "licgrpc").await;

    let body_for = |tag: &str| {
        serde_json::json!({
            "name": format!("{PREFIX}-create-{tag}"),
            "display_name": "Created Group",
            "source": "custom",
            "allow_reference": true,
            "description": "made by the parity suite",
            "user_ids": [plain.id.as_str()],
        })
        .to_string()
    };
    let (go_status, go, _) = send(
        &client,
        &pair.go,
        &admin,
        reqwest::Method::POST,
        "/api/v4/groups",
        &body_for("go"),
    )
    .await;
    let (rs_status, rs, served) = send(
        &client,
        &pair.rust,
        &admin,
        reqwest::Method::POST,
        "/api/v4/groups",
        &body_for("rs"),
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(go_status, 201, "{}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, 201, "{}", String::from_utf8_lossy(&rs));
    let go_group = parse(&go);
    let rs_group = parse(&rs);
    assert_eq!(masked_group(&go_group), masked_group(&rs_group));
    assert_eq!(rs_group["member_count"], 1, "the one member counts");
    assert_eq!(rs_group["member_ids"], serde_json::Value::Null);
    assert_eq!(rs_group["has_syncables"], false);
    assert_eq!(rs_group["remote_id"], serde_json::Value::Null);
    assert_eq!(rs_group["name"], format!("{PREFIX}-create-rs"));
    assert!(!rs.ends_with(b"\n"), "`w.Write` — no newline");
    assert!(!go.ends_with(b"\n"));
    assert_eq!(
        rs_group["id"].as_str().map(str::len),
        Some(26),
        "a minted id"
    );

    delete_plain_user(&client, &admin, &plain.id).await;
    purge().await;
}

/// The refusals in front of the insert, each put to both servers with the same body and
/// compared by status and id. The order matters and is measured: `source` before the licence
/// tier, the permission before `allow_reference`, and the body before all of them.
#[tokio::test]
async fn create_refuses_in_gos_order() {
    if !stack_enabled() {
        return;
    }
    let _suite = SUITE.lock().await;
    purge().await;
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "licgrpr").await;
    let admin_username: String = client
        .get(format!("{GO}/api/v4/users/me"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("me")
        .json::<serde_json::Value>()
        .await
        .expect("json")["username"]
        .as_str()
        .unwrap()
        .to_owned();

    let cases: Vec<(&str, &str, String, u16, &str)> = vec![
        (
            "malformed",
            &admin,
            "{".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "null",
            &admin,
            "null".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "empty",
            &admin,
            String::new(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "ldap_source",
            &admin,
            format!(
                r#"{{"name":"{PREFIX}-ldap","display_name":"x","source":"ldap","allow_reference":true,"remote_id":"r"}}"#
            ),
            400,
            "app.group.crud_permission",
        ),
        (
            "not_referenceable",
            &admin,
            format!(
                r#"{{"name":"{PREFIX}-noref","display_name":"x","source":"custom","allow_reference":false}}"#
            ),
            400,
            "api.custom_groups.must_be_referenceable",
        ),
        (
            "remote_id",
            &admin,
            format!(
                r#"{{"name":"{PREFIX}-remote","display_name":"x","source":"custom","allow_reference":true,"remote_id":"r"}}"#
            ),
            400,
            "api.custom_groups.no_remote_id",
        ),
        (
            "name_is_a_username",
            &admin,
            format!(
                r#"{{"name":"{admin_username}","display_name":"x","source":"custom","allow_reference":true}}"#
            ),
            400,
            "app.group.username_conflict",
        ),
        (
            "invalid_name",
            &admin,
            format!(
                r#"{{"name":"{PREFIX} with spaces","display_name":"x","source":"custom","allow_reference":true}}"#
            ),
            400,
            "model.group.name.invalid_chars.app_error",
        ),
        (
            "missing_display_name",
            &admin,
            format!(r#"{{"name":"{PREFIX}-nodisplay","source":"custom","allow_reference":true}}"#),
            400,
            "model.group.display_name.app_error",
        ),
        (
            "unknown_member",
            &admin,
            format!(
                r#"{{"name":"{PREFIX}-unknown","display_name":"x","source":"custom","allow_reference":true,"user_ids":["{NOWHERE}"]}}"#
            ),
            500,
            "app.insert_error",
        ),
    ];
    for (what, token, body, status, id) in &cases {
        let ((go_status, go), (rs_status, rs)) = both(
            &client,
            &pair,
            token,
            reqwest::Method::POST,
            "/api/v4/groups",
            body,
        )
        .await;
        assert_eq!(
            go_status,
            *status,
            "{what}: Go: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{what}");
        assert_eq!(same_error(&go, &rs, what), *id, "{what}");
    }

    // A plain user: `create_custom_group` is granted to `system_user` in the stock role set, so
    // this succeeds — and a roleless user, below, is the 403.
    let body_for = |tag: &str| {
        format!(
            r#"{{"name":"{PREFIX}-plain-{tag}","display_name":"Plain Made","source":"custom","allow_reference":true}}"#
        )
    };
    let (go_status, go, _) = send(
        &client,
        &pair.go,
        &plain.token,
        reqwest::Method::POST,
        "/api/v4/groups",
        &body_for("go"),
    )
    .await;
    let (rs_status, rs, served) = send(
        &client,
        &pair.rust,
        &plain.token,
        reqwest::Method::POST,
        "/api/v4/groups",
        &body_for("rs"),
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(
        rs_status,
        go_status,
        "a plain user creating: Go {}",
        String::from_utf8_lossy(&go)
    );
    if go_status == 201 {
        assert_eq!(masked_group(&parse(&go)), masked_group(&parse(&rs)));
    } else {
        same_error(&go, &rs, "plain user create");
    }

    // A taken name: the second create of the same name on each server is the unique-name 400.
    for (base, tag) in [(&pair.go, "go"), (&pair.rust, "rs")] {
        let body = format!(
            r#"{{"name":"{PREFIX}-dup-{tag}","display_name":"x","source":"custom","allow_reference":true}}"#
        );
        let (first, _, _) = send(
            &client,
            base,
            &admin,
            reqwest::Method::POST,
            "/api/v4/groups",
            &body,
        )
        .await;
        assert_eq!(first, 201, "{tag}: first create");
    }
    let ((go_status, go), (rs_status, rs)) = both(
        &client,
        &pair,
        &admin,
        reqwest::Method::POST,
        "/api/v4/groups",
        &format!(r#"{{"name":"{PREFIX}-dup-go","display_name":"x","source":"custom","allow_reference":true}}"#),
    )
    .await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, 400);
    assert_eq!(
        same_error(&go, &rs, "duplicate name"),
        "app.custom_group.unique_name"
    );

    let roleless = roleless_user(&client, &admin, &team, "licgrpz").await;
    let ((go_status, go), (rs_status, rs)) = both(
        &client,
        &pair,
        &roleless.token,
        reqwest::Method::POST,
        "/api/v4/groups",
        &body_for("roleless"),
    )
    .await;
    assert_eq!(go_status, 403, "{}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, 403);
    assert_eq!(
        same_error(&go, &rs, "roleless create"),
        "api.context.permissions.app_error"
    );

    delete_plain_user(&client, &admin, &roleless.id).await;
    delete_plain_user(&client, &admin, &plain.id).await;
    purge().await;
}

// ---------------------------------------------------------------------------------------------
// getGroupsByNames
// ---------------------------------------------------------------------------------------------

/// A read, so one fixture and identical bytes: an empty list is a literal `[]` before any
/// permission question, a parse failure is `PayloadParseError`, a plain user does not see a
/// group that is not referenceable and an administrator does, and a deleted group is still
/// found by name.
#[tokio::test]
async fn names_are_looked_up_identically() {
    if !stack_enabled() {
        return;
    }
    let _suite = SUITE.lock().await;
    purge().await;
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team, "licgrpn").await;

    let visible = create_via_go(&client, &pair, &admin, "names-visible", &[]).await;
    plant_group(HIDDEN_GROUP, "names-hidden", "custom", false).await;
    let deleted = create_via_go(&client, &pair, &admin, "names-deleted", &[]).await;
    let (status, _, _) = send(
        &client,
        &pair.go,
        &admin,
        reqwest::Method::DELETE,
        &format!("/api/v4/groups/{}", deleted["id"].as_str().unwrap()),
        "",
    )
    .await;
    assert_eq!(status, 200, "the fixture is deleted through Go");

    let path = "/api/v4/groups/names";
    let all = format!(
        r#"["{PREFIX}-names-visible","{PREFIX}-names-hidden","{PREFIX}-names-deleted","{PREFIX}-names-visible","nope"]"#
    );
    for (what, token, body, status) in [
        ("empty", &admin, "[]".to_owned(), 200),
        ("admin", &admin, all.clone(), 200),
        ("plain", &plain.token, all.clone(), 200),
        ("malformed", &admin, "[".to_owned(), 400),
        ("not_an_array", &admin, r#"{"a":1}"#.to_owned(), 400),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            both(&client, &pair, token, reqwest::Method::POST, path, &body).await;
        assert_eq!(
            go_status,
            status,
            "{what}: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{what}");
        if status == 200 {
            assert_eq!(
                String::from_utf8_lossy(&go),
                String::from_utf8_lossy(&rs),
                "{what}: identical bytes"
            );
            assert!(!rs.ends_with(b"\n"), "{what}: no newline");
        } else {
            assert_eq!(
                same_error(&go, &rs, what),
                "api.payload.parse.error",
                "{what}"
            );
        }
    }
    let ((_, go_admin), _) = both(&client, &pair, &admin, reqwest::Method::POST, path, &all).await;
    let ((_, go_plain), _) = both(
        &client,
        &pair,
        &plain.token,
        reqwest::Method::POST,
        path,
        &all,
    )
    .await;
    let names = |bytes: &[u8]| -> Vec<String> {
        parse(bytes)
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let admin_names = names(&go_admin);
    let plain_names = names(&go_plain);
    assert!(
        admin_names.contains(&format!("{PREFIX}-names-hidden")),
        "an admin sees the unreferenceable group"
    );
    assert!(
        !plain_names.contains(&format!("{PREFIX}-names-hidden")),
        "a plain user does not"
    );
    assert!(
        plain_names.contains(&format!("{PREFIX}-names-deleted")),
        "a deleted group is still found by name"
    );
    assert_eq!(admin_names.len(), 3, "each name once: {admin_names:?}");
    assert_eq!(visible["name"], format!("{PREFIX}-names-visible"));

    delete_plain_user(&client, &admin, &plain.id).await;
    purge().await;
}

// ---------------------------------------------------------------------------------------------
// patchGroup
// ---------------------------------------------------------------------------------------------

/// The refusals of `patchGroup`, in Go's order, and the name derivation. Each case is one
/// request to both servers on the same fixture; the two `display_name` patches that succeed are
/// compared masked, because each moves `update_at`.
#[tokio::test]
async fn patch_refuses_derives_and_updates_identically() {
    if !stack_enabled() {
        return;
    }
    let _suite = SUITE.lock().await;
    purge().await;
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let member = roleless_user(&client, &admin, &team, "licgrpm").await;
    let outsider = roleless_user(&client, &admin, &team, "licgrpo").await;

    let group = create_via_go(&client, &pair, &admin, "patch-target", &[&member.id]).await;
    let group_id = group["id"].as_str().unwrap().to_owned();
    let other = create_via_go(&client, &pair, &admin, "patch-other", &[]).await;
    plant_group(HIDDEN_GROUP, "patch-hidden", "custom", false).await;
    plant_group(LDAP_GROUP, "patch-ldap", "ldap", true).await;
    let path = format!("/api/v4/groups/{group_id}/patch");

    let cases: Vec<(&str, &str, String, String, u16, &str)> = vec![
        (
            "bad_id",
            &admin,
            "/api/v4/groups/abc/patch".to_owned(),
            "{}".to_owned(),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "missing",
            &admin,
            format!("/api/v4/groups/{NOWHERE}/patch"),
            "{}".to_owned(),
            404,
            "app.group.no_rows",
        ),
        (
            "outsider",
            &outsider.token,
            path.clone(),
            r#"{"display_name":"x"}"#.to_owned(),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "malformed",
            &admin,
            path.clone(),
            "{".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "unreferenceable",
            &admin,
            path.clone(),
            r#"{"allow_reference":false}"#.to_owned(),
            400,
            "api.custom_groups.must_be_referenceable",
        ),
        (
            "reserved_name",
            &admin,
            path.clone(),
            r#"{"allow_reference":true,"name":"all"}"#.to_owned(),
            400,
            "api.ldap_groups.existing_reserved_name_error",
        ),
        (
            "channel_name",
            &admin,
            path.clone(),
            r#"{"allow_reference":true,"name":"channel"}"#.to_owned(),
            400,
            "api.ldap_groups.existing_reserved_name_error",
        ),
        (
            "username",
            &admin,
            path.clone(),
            r#"{"allow_reference":true,"name":"sliceuser"}"#.to_owned(),
            400,
            "api.ldap_groups.existing_user_name_error",
        ),
        (
            "other_groups_name",
            &admin,
            path.clone(),
            format!(r#"{{"allow_reference":true,"name":"{PREFIX}-patch-other"}}"#),
            400,
            "api.ldap_groups.existing_group_name_error",
        ),
        // The group's **own** name is refused too: the collision check does not exempt it.
        (
            "own_name",
            &admin,
            path.clone(),
            format!(r#"{{"allow_reference":true,"name":"{PREFIX}-patch-target"}}"#),
            400,
            "api.ldap_groups.existing_group_name_error",
        ),
        // A name held by an unreferenceable group passes the check and fails on the constraint.
        (
            "hidden_groups_name",
            &admin,
            path.clone(),
            format!(r#"{{"allow_reference":true,"name":"{PREFIX}-patch-hidden"}}"#),
            400,
            "app.custom_group.unique_name",
        ),
        (
            "invalid_name",
            &admin,
            path.clone(),
            r#"{"name":"Has Spaces"}"#.to_owned(),
            400,
            "model.group.name.invalid_chars.app_error",
        ),
        (
            "empty_display_name",
            &admin,
            path.clone(),
            r#"{"display_name":""}"#.to_owned(),
            400,
            "model.group.display_name.app_error",
        ),
    ];
    for (what, token, p, body, status, id) in &cases {
        let ((go_status, go), (rs_status, rs)) =
            both(&client, &pair, token, reqwest::Method::PUT, p, body).await;
        assert_eq!(
            go_status,
            *status,
            "{what}: Go: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{what}");
        assert_eq!(same_error(&go, &rs, what), *id, "{what}");
    }

    // A member with no roles of their own. With the **stock** `custom_group_user` role — which
    // has no permissions — membership grants nothing and they are refused like the outsider.
    let body = r#"{"description":"patched by a member"}"#;
    let ((go_status, go), (rs_status, rs)) = both(
        &client,
        &pair,
        &member.token,
        reqwest::Method::PUT,
        &path,
        body,
    )
    .await;
    assert_eq!(
        go_status,
        403,
        "stock role: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, 403);
    assert_eq!(
        same_error(&go, &rs, "member, stock role"),
        "api.context.permissions.app_error"
    );

    // Give the role `edit_custom_group`: now membership is what lets them through, and the
    // outsider — same roles, no membership — is still refused. The description changes on both
    // servers, one after the other, and the second answer differs only in `update_at`.
    // Exclusive on the built-in rows from the edit to the restore: `roles` byte-compares this
    // role, `update_at` included, and must not fetch one server on each side of the patch.
    let role_rows = common::ROLE_ROWS.write().await;
    set_custom_group_user_permissions(&client, &pair, &admin, &["edit_custom_group"]).await;
    let ((go_status, go), (rs_status, rs)) = both(
        &client,
        &pair,
        &outsider.token,
        reqwest::Method::PUT,
        &path,
        body,
    )
    .await;
    assert_eq!(
        go_status,
        403,
        "outsider, role granted: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(rs_status, 403);
    assert_eq!(
        same_error(&go, &rs, "outsider, granted role"),
        "api.context.permissions.app_error"
    );
    let ((go_status, go), (rs_status, rs)) = both(
        &client,
        &pair,
        &member.token,
        reqwest::Method::PUT,
        &path,
        body,
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs));
    let go_group = parse(&go);
    let rs_group = parse(&rs);
    assert_eq!(masked_group(&go_group), masked_group(&rs_group));
    assert_eq!(rs_group["description"], "patched by a member");
    assert_eq!(rs_group["member_count"], 1);
    assert_eq!(rs_group["id"], group_id);
    assert!(rs_group["update_at"].as_i64().unwrap() >= go_group["update_at"].as_i64().unwrap());
    assert!(!rs.ends_with(b"\n"));
    set_custom_group_user_permissions(&client, &pair, &admin, &[]).await;
    drop(role_rows);

    // Derivation: `allow_reference: true` with no name lower-cases the display name and turns
    // spaces into hyphens. Done on two different groups so each server derives once.
    let derive_go = create_via_go(&client, &pair, &admin, "derive-go", &[]).await;
    let derive_rs = create_via_go(&client, &pair, &admin, "derive-rs", &[]).await;
    for (base, g, tag) in [(&pair.go, &derive_go, "go"), (&pair.rust, &derive_rs, "rs")] {
        let p = format!("/api/v4/groups/{}/patch", g["id"].as_str().unwrap());
        // First give it a display name with mixed case and two spaces; the tag keeps the two
        // derived names apart, since a name is unique across groups, and the prefix survives
        // the derivation so `purge` still finds the row under its new name.
        let (status, bytes, _) = send(
            &client,
            base,
            &admin,
            reqwest::Method::PUT,
            &p,
            &format!(r#"{{"display_name":"Mmrslicgrp  Mixed CASE {tag}"}}"#),
        )
        .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        let (status, bytes, _) = send(
            &client,
            base,
            &admin,
            reqwest::Method::PUT,
            &p,
            r#"{"allow_reference":true}"#,
        )
        .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        assert_eq!(
            parse(&bytes)["name"],
            format!("{PREFIX}--mixed-case-{tag}"),
            "each space becomes a hyphen, case folds"
        );
    }

    // An `ldap` group: the source picks `sysconsole_write_user_management_groups`, which the
    // administrator holds and the member does not, and `LDAPGroups` is on in the licence.
    let ldap_path = format!("/api/v4/groups/{LDAP_GROUP}/patch");
    let ((go_status, go), (rs_status, rs)) = both(
        &client,
        &pair,
        &member.token,
        reqwest::Method::PUT,
        &ldap_path,
        r#"{"description":"x"}"#,
    )
    .await;
    assert_eq!(go_status, 403, "{}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, 403);
    assert_eq!(
        same_error(&go, &rs, "ldap member"),
        "api.context.permissions.app_error"
    );
    let ((go_status, go), (rs_status, rs)) = both(
        &client,
        &pair,
        &admin,
        reqwest::Method::PUT,
        &ldap_path,
        r#"{"description":"ldap patched"}"#,
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs));
    assert_eq!(masked_group(&parse(&go)), masked_group(&parse(&rs)));
    assert_eq!(other["source"], "custom");

    delete_plain_user(&client, &admin, &member.id).await;
    delete_plain_user(&client, &admin, &outsider.id).await;
    purge().await;
}

// ---------------------------------------------------------------------------------------------
// deleteGroup / restoreGroup
// ---------------------------------------------------------------------------------------------

/// The two soft-delete mirrors: a delete answers the group with `delete_at` set, a second delete
/// is a 404, a restore answers it with `delete_at` back to 0, a restore of a live group is a 404;
/// a non-custom group is `crud_permission` at **400** for delete and **501** for restore; an
/// outsider is 403 on both; `DELETE /groups/names` is the id refusal.
#[tokio::test]
async fn delete_and_restore_mirror_each_other() {
    if !stack_enabled() {
        return;
    }
    let _suite = SUITE.lock().await;
    purge().await;
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let outsider = roleless_user(&client, &admin, &team, "licgrpd").await;
    plant_group(LDAP_GROUP, "delete-ldap", "ldap", true).await;

    // One fixture per server for the successful path.
    let for_go = create_via_go(&client, &pair, &admin, "delete-go", &[]).await;
    let for_rs = create_via_go(&client, &pair, &admin, "delete-rs", &[]).await;
    let go_id = for_go["id"].as_str().unwrap();
    let rs_id = for_rs["id"].as_str().unwrap();

    let delete = reqwest::Method::DELETE;
    let post = reqwest::Method::POST;
    let (s1, go_deleted, _) = send(
        &client,
        &pair.go,
        &admin,
        delete.clone(),
        &format!("/api/v4/groups/{go_id}"),
        "",
    )
    .await;
    let (s2, rs_deleted, served) = send(
        &client,
        &pair.rust,
        &admin,
        delete.clone(),
        &format!("/api/v4/groups/{rs_id}"),
        "",
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(
        (s1, s2),
        (200, 200),
        "{} / {}",
        String::from_utf8_lossy(&go_deleted),
        String::from_utf8_lossy(&rs_deleted)
    );
    let go_deleted = parse(&go_deleted);
    let rs_deleted = parse(&rs_deleted);
    assert_eq!(masked_group(&go_deleted), masked_group(&rs_deleted));
    assert!(rs_deleted["delete_at"].as_i64().unwrap() > 0);
    assert_eq!(
        rs_deleted["delete_at"], rs_deleted["update_at"],
        "one clock reading for both"
    );
    assert_eq!(rs_deleted["member_count"], 0);

    // Deleting again: 404 on both (each on its own already-deleted fixture).
    let (s1, go, _) = send(
        &client,
        &pair.go,
        &admin,
        delete.clone(),
        &format!("/api/v4/groups/{go_id}"),
        "",
    )
    .await;
    let (s2, rs, _) = send(
        &client,
        &pair.rust,
        &admin,
        delete.clone(),
        &format!("/api/v4/groups/{rs_id}"),
        "",
    )
    .await;
    assert_eq!((s1, s2), (404, 404));
    assert_eq!(same_error(&go, &rs, "double delete"), "app.group.no_rows");

    // Restore both.
    let (s1, go, _) = send(
        &client,
        &pair.go,
        &admin,
        post.clone(),
        &format!("/api/v4/groups/{go_id}/restore"),
        "",
    )
    .await;
    let (s2, rs, served) = send(
        &client,
        &pair.rust,
        &admin,
        post.clone(),
        &format!("/api/v4/groups/{rs_id}/restore"),
        "",
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!((s1, s2), (200, 200), "{}", String::from_utf8_lossy(&rs));
    let rs_restored = parse(&rs);
    assert_eq!(masked_group(&parse(&go)), masked_group(&rs_restored));
    assert_eq!(rs_restored["delete_at"], 0);

    // Restoring a live group: 404 on both.
    let (s1, go, _) = send(
        &client,
        &pair.go,
        &admin,
        post.clone(),
        &format!("/api/v4/groups/{go_id}/restore"),
        "",
    )
    .await;
    let (s2, rs, _) = send(
        &client,
        &pair.rust,
        &admin,
        post.clone(),
        &format!("/api/v4/groups/{rs_id}/restore"),
        "",
    )
    .await;
    assert_eq!((s1, s2), (404, 404));
    assert_eq!(same_error(&go, &rs, "restore live"), "app.group.no_rows");

    // Same request to both for the refusals.
    let cases: Vec<(&str, &str, reqwest::Method, String, u16, &str)> = vec![
        (
            "delete_names",
            &admin,
            delete.clone(),
            "/api/v4/groups/names".to_owned(),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "delete_missing",
            &admin,
            delete.clone(),
            format!("/api/v4/groups/{NOWHERE}"),
            404,
            "app.group.no_rows",
        ),
        (
            "restore_missing",
            &admin,
            post.clone(),
            format!("/api/v4/groups/{NOWHERE}/restore"),
            404,
            "app.group.no_rows",
        ),
        (
            "delete_ldap",
            &admin,
            delete.clone(),
            format!("/api/v4/groups/{LDAP_GROUP}"),
            400,
            "app.group.crud_permission",
        ),
        (
            "restore_ldap",
            &admin,
            post.clone(),
            format!("/api/v4/groups/{LDAP_GROUP}/restore"),
            501,
            "app.group.crud_permission",
        ),
        (
            "delete_outsider",
            &outsider.token,
            delete.clone(),
            format!("/api/v4/groups/{go_id}"),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "restore_outsider",
            &outsider.token,
            post.clone(),
            format!("/api/v4/groups/{go_id}/restore"),
            403,
            "api.context.permissions.app_error",
        ),
    ];
    for (what, token, method, p, status, id) in &cases {
        let ((go_status, go), (rs_status, rs)) =
            both(&client, &pair, token, method.clone(), p, "").await;
        assert_eq!(
            go_status,
            *status,
            "{what}: Go: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{what}");
        assert_eq!(same_error(&go, &rs, what), *id, "{what}");
    }

    delete_plain_user(&client, &admin, &outsider.id).await;
    purge().await;
}

// ---------------------------------------------------------------------------------------------
// addGroupMembers / deleteGroupMembers
// ---------------------------------------------------------------------------------------------

/// Membership writes: the list comes back in request order with one `create_at`; an id that is
/// not an id is the body refusal, an id naming nobody is `user_not_found`, an id listed twice is
/// a 500, and removing a non-member — or a member already removed — is `user_not_found` too.
/// An empty or absent list is an empty answer, whose exact bytes are measured rather than assumed.
#[tokio::test]
async fn membership_writes_match_go() {
    if !stack_enabled() {
        return;
    }
    let _suite = SUITE.lock().await;
    purge().await;
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let u1 = create_plain_user(&client, &admin, &team, "licgrpu1").await;
    let u2 = create_plain_user(&client, &admin, &team, "licgrpu2").await;
    let outsider = roleless_user(&client, &admin, &team, "licgrpu3").await;

    let for_go = create_via_go(&client, &pair, &admin, "members-go", &[]).await;
    let for_rs = create_via_go(&client, &pair, &admin, "members-rs", &[]).await;
    let go_id = for_go["id"].as_str().unwrap();
    let rs_id = for_rs["id"].as_str().unwrap();
    let post = reqwest::Method::POST;
    let delete = reqwest::Method::DELETE;
    let two = format!(r#"{{"user_ids":["{}","{}"]}}"#, u2.id, u1.id);

    // Add two, in an order that is not creation order, and expect that order back.
    let (s1, go, _) = send(
        &client,
        &pair.go,
        &admin,
        post.clone(),
        &format!("/api/v4/groups/{go_id}/members"),
        &two,
    )
    .await;
    let (s2, rs, served) = send(
        &client,
        &pair.rust,
        &admin,
        post.clone(),
        &format!("/api/v4/groups/{rs_id}/members"),
        &two,
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(
        (s1, s2),
        (200, 200),
        "{} / {}",
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs)
    );
    let go_members = parse(&go);
    let rs_members = parse(&rs);
    let strip_group = |v: &serde_json::Value| {
        let mut v = masked_members(v);
        for m in v.as_array_mut().unwrap() {
            m.as_object_mut()
                .unwrap()
                .insert("group_id".into(), "<masked>".into());
        }
        v
    };
    assert_eq!(strip_group(&go_members), strip_group(&rs_members));
    assert_eq!(
        rs_members[0]["user_id"], u2.id,
        "request order, not creation order"
    );
    assert_eq!(rs_members[1]["user_id"], u1.id);
    assert_eq!(
        rs_members[0]["create_at"], rs_members[1]["create_at"],
        "one clock reading"
    );
    assert_eq!(rs_members[0]["group_id"], rs_id);
    assert!(!rs.ends_with(b"\n"));

    // Re-adding is an upsert: the same answer again, and still two members.
    let (s2, rs, _) = send(
        &client,
        &pair.rust,
        &admin,
        post.clone(),
        &format!("/api/v4/groups/{rs_id}/members"),
        &two,
    )
    .await;
    assert_eq!(s2, 200);
    let (s1, go, _) = send(
        &client,
        &pair.go,
        &admin,
        post.clone(),
        &format!("/api/v4/groups/{go_id}/members"),
        &two,
    )
    .await;
    assert_eq!(s1, 200);
    assert_eq!(strip_group(&parse(&go)), strip_group(&parse(&rs)));

    // Refusals, same body to both on the Go fixture (nothing below writes).
    let mpath = format!("/api/v4/groups/{go_id}/members");
    let cases: Vec<(&str, &str, reqwest::Method, String, String, u16, &str)> = vec![
        (
            "add_bad_id",
            &admin,
            post.clone(),
            "/api/v4/groups/abc/members".to_owned(),
            two.clone(),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "add_missing_group",
            &admin,
            post.clone(),
            format!("/api/v4/groups/{NOWHERE}/members"),
            two.clone(),
            404,
            "app.group.no_rows",
        ),
        (
            "add_ldap",
            &admin,
            post.clone(),
            format!("/api/v4/groups/{LDAP_GROUP}/members"),
            two.clone(),
            400,
            "app.group.crud_permission",
        ),
        (
            "add_outsider",
            &outsider.token,
            post.clone(),
            mpath.clone(),
            two.clone(),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "add_malformed",
            &admin,
            post.clone(),
            mpath.clone(),
            "{".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "add_null",
            &admin,
            post.clone(),
            mpath.clone(),
            "null".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "add_not_an_id",
            &admin,
            post.clone(),
            mpath.clone(),
            r#"{"user_ids":["abc"]}"#.to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "add_nobody",
            &admin,
            post.clone(),
            mpath.clone(),
            format!(r#"{{"user_ids":["{NOWHERE}"]}}"#),
            400,
            "app.group.user_not_found",
        ),
        (
            "add_twice",
            &admin,
            post.clone(),
            mpath.clone(),
            format!(r#"{{"user_ids":["{0}","{0}"]}}"#, outsider.id),
            500,
            "app.update_error",
        ),
        (
            "remove_outsider",
            &outsider.token,
            delete.clone(),
            mpath.clone(),
            two.clone(),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "remove_not_an_id",
            &admin,
            delete.clone(),
            mpath.clone(),
            r#"{"user_ids":["abc"]}"#.to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "remove_non_member",
            &admin,
            delete.clone(),
            mpath.clone(),
            format!(r#"{{"user_ids":["{}"]}}"#, outsider.id),
            400,
            "app.group.user_not_found",
        ),
        (
            "remove_ldap",
            &admin,
            delete.clone(),
            format!("/api/v4/groups/{LDAP_GROUP}/members"),
            two.clone(),
            400,
            "app.group.crud_permission",
        ),
    ];
    plant_group(LDAP_GROUP, "members-ldap", "ldap", true).await;
    for (what, token, method, p, body, status, id) in &cases {
        let ((go_status, go), (rs_status, rs)) =
            both(&client, &pair, token, method.clone(), p, body).await;
        assert_eq!(
            go_status,
            *status,
            "{what}: Go: {}",
            String::from_utf8_lossy(&go)
        );
        assert_eq!(rs_status, go_status, "{what}");
        assert_eq!(same_error(&go, &rs, what), *id, "{what}");
    }

    // The empty shapes: measured bytes, identical on both.
    for (what, method, body) in [
        ("add_empty_object", post.clone(), "{}"),
        ("add_null_ids", post.clone(), r#"{"user_ids":null}"#),
        ("add_empty_list", post.clone(), r#"{"user_ids":[]}"#),
        ("remove_empty_object", delete.clone(), "{}"),
        ("remove_empty_list", delete.clone(), r#"{"user_ids":[]}"#),
    ] {
        let ((go_status, go), (rs_status, rs)) =
            both(&client, &pair, &admin, method, &mpath, body).await;
        assert_eq!(go_status, 200, "{what}: {}", String::from_utf8_lossy(&go));
        assert_eq!(rs_status, 200, "{what}: {}", String::from_utf8_lossy(&rs));
        assert_eq!(
            String::from_utf8_lossy(&go),
            String::from_utf8_lossy(&rs),
            "{what}"
        );
    }

    // Remove one on each side, in a list that names it and a member together with a duplicate:
    // Go compares lengths first, so `[u1, u1]` against one row passes the check.
    let one_twice = format!(r#"{{"user_ids":["{0}","{0}"]}}"#, u1.id);
    let (s1, go, _) = send(
        &client,
        &pair.go,
        &admin,
        delete.clone(),
        &format!("/api/v4/groups/{go_id}/members"),
        &one_twice,
    )
    .await;
    let (s2, rs, _) = send(
        &client,
        &pair.rust,
        &admin,
        delete.clone(),
        &format!("/api/v4/groups/{rs_id}/members"),
        &one_twice,
    )
    .await;
    assert_eq!(
        (s1, s2),
        (200, 200),
        "{} / {}",
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs)
    );
    let rs_removed = parse(&rs);
    assert_eq!(strip_group(&parse(&go)), strip_group(&rs_removed));
    assert_eq!(
        rs_removed.as_array().unwrap().len(),
        1,
        "one row, named twice"
    );
    assert!(rs_removed[0]["delete_at"].as_i64().unwrap() > 0);

    // Removing it again: already removed is not found.
    let (s1, go, _) = send(
        &client,
        &pair.go,
        &admin,
        delete.clone(),
        &format!("/api/v4/groups/{go_id}/members"),
        &one_twice,
    )
    .await;
    let (s2, rs, _) = send(
        &client,
        &pair.rust,
        &admin,
        delete.clone(),
        &format!("/api/v4/groups/{rs_id}/members"),
        &one_twice,
    )
    .await;
    assert_eq!((s1, s2), (400, 400));
    assert_eq!(
        same_error(&go, &rs, "remove twice"),
        "app.group.user_not_found"
    );

    // And the member count the group reports agrees: one left on each.
    assert_eq!(member_count(&client, &pair.go, &admin, go_id).await, 1);
    assert_eq!(member_count(&client, &pair.rust, &admin, rs_id).await, 1);

    // Re-adding the removed member is the `ON CONFLICT … DO UPDATE` half of the upsert: the
    // soft-deleted row comes back to life and the count is two again on both.
    let one = format!(r#"{{"user_ids":["{}"]}}"#, u1.id);
    let (s1, _, _) = send(
        &client,
        &pair.go,
        &admin,
        post.clone(),
        &format!("/api/v4/groups/{go_id}/members"),
        &one,
    )
    .await;
    let (s2, _, _) = send(
        &client,
        &pair.rust,
        &admin,
        post.clone(),
        &format!("/api/v4/groups/{rs_id}/members"),
        &one,
    )
    .await;
    assert_eq!((s1, s2), (200, 200));
    assert_eq!(member_count(&client, &pair.go, &admin, go_id).await, 2);
    assert_eq!(member_count(&client, &pair.rust, &admin, rs_id).await, 2);

    // A deactivated member is not counted — `Users.DeleteAt = 0` in the count — while the
    // membership row stays. Deactivated through Go, which both servers then see.
    let deactivated = client
        .delete(format!("{GO}/api/v4/users/{}", u2.id))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("deactivate");
    assert_eq!(deactivated.status(), 200);
    assert_eq!(member_count(&client, &pair.go, &admin, go_id).await, 1);
    assert_eq!(member_count(&client, &pair.rust, &admin, rs_id).await, 1);

    for u in [&u1, &u2, &outsider] {
        delete_plain_user(&client, &admin, &u.id).await;
    }
    purge().await;
}

// ---------------------------------------------------------------------------------------------
// websocket events
// ---------------------------------------------------------------------------------------------

/// `received_group` on create, patch, delete and restore — to everyone, the group as a JSON
/// **string** under `data.group` — and `group_member_add` / `group_member_deleted` addressed to
/// the member alone, the row as a string under `data.group_member`. The administrator is the
/// member here so their own socket sees both.
#[tokio::test]
async fn the_three_events_reach_the_socket_with_string_payloads() {
    if !stack_enabled() {
        return;
    }
    let _suite = SUITE.lock().await;
    purge().await;
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let me = logged_in_user_id();
    let _stream = common::BROADCAST_STREAM.lock().await;

    let mut go_probe = SocketProbe::connect(&pair.go, &admin).await;
    let mut rs_probe = SocketProbe::connect(&pair.rust, &admin).await;

    let group_from = |frame: &serde_json::Value| -> serde_json::Value {
        let text = frame["data"]["group"]
            .as_str()
            .expect("`group` is a JSON string");
        serde_json::from_str(text).expect("which parses")
    };
    let member_from = |frame: &serde_json::Value| -> serde_json::Value {
        let text = frame["data"]["group_member"]
            .as_str()
            .expect("`group_member` is a JSON string");
        serde_json::from_str(text).expect("which parses")
    };

    let body_for = |tag: &str| {
        format!(
            r#"{{"name":"{PREFIX}-ws-{tag}","display_name":"Socket {tag}","source":"custom","allow_reference":true}}"#
        )
    };
    let (s1, go, _) = send(
        &client,
        &pair.go,
        &admin,
        reqwest::Method::POST,
        "/api/v4/groups",
        &body_for("go"),
    )
    .await;
    let (s2, rs, _) = send(
        &client,
        &pair.rust,
        &admin,
        reqwest::Method::POST,
        "/api/v4/groups",
        &body_for("rs"),
    )
    .await;
    assert_eq!(
        (s1, s2),
        (201, 201),
        "{} / {}",
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs)
    );
    let go_group = parse(&go);
    let rs_group = parse(&rs);
    let go_id = go_group["id"].as_str().unwrap().to_owned();
    let rs_id = rs_group["id"].as_str().unwrap().to_owned();

    let window = Duration::from_secs(5);
    let names = |v: &serde_json::Value, id: &str| {
        v["event"] == "received_group"
            && v["data"]["group"].as_str().is_some_and(|g| g.contains(id))
    };
    assert!(
        go_probe
            .collect_until(window, |f| f.iter().any(|v| names(v, &go_id)))
            .await,
        "Go publishes received_group"
    );
    assert!(
        rs_probe
            .collect_until(window, |f| f.iter().any(|v| names(v, &rs_id)))
            .await,
        "we publish received_group"
    );
    let go_frame = go_probe
        .events_named("received_group")
        .into_iter()
        .find(|v| names(v, &go_id))
        .unwrap();
    let rs_frame = rs_probe
        .events_named("received_group")
        .into_iter()
        .find(|v| names(v, &rs_id))
        .unwrap();
    assert_eq!(
        masked_group(&group_from(&go_frame)),
        masked_group(&group_from(&rs_frame))
    );
    assert_eq!(
        group_from(&rs_frame),
        rs_group,
        "the payload is the response body"
    );
    assert_eq!(
        go_frame["broadcast"], rs_frame["broadcast"],
        "no team, channel or user in the broadcast"
    );
    assert_eq!(rs_frame["broadcast"]["user_id"], "");

    // Add the administrator as a member: `group_member_add`, addressed to them.
    let body = format!(r#"{{"user_ids":["{me}"]}}"#);
    let (s1, _, _) = send(
        &client,
        &pair.go,
        &admin,
        reqwest::Method::POST,
        &format!("/api/v4/groups/{go_id}/members"),
        &body,
    )
    .await;
    let (s2, _, _) = send(
        &client,
        &pair.rust,
        &admin,
        reqwest::Method::POST,
        &format!("/api/v4/groups/{rs_id}/members"),
        &body,
    )
    .await;
    assert_eq!((s1, s2), (200, 200));
    let added = |v: &serde_json::Value, id: &str| {
        v["event"] == "group_member_add"
            && v["data"]["group_member"]
                .as_str()
                .is_some_and(|g| g.contains(id))
    };
    assert!(
        go_probe
            .collect_until(window, |f| f.iter().any(|v| added(v, &go_id)))
            .await
    );
    assert!(
        rs_probe
            .collect_until(window, |f| f.iter().any(|v| added(v, &rs_id)))
            .await
    );
    let go_frame = go_probe
        .events_named("group_member_add")
        .into_iter()
        .find(|v| added(v, &go_id))
        .unwrap();
    let rs_frame = rs_probe
        .events_named("group_member_add")
        .into_iter()
        .find(|v| added(v, &rs_id))
        .unwrap();
    let mut go_member = member_from(&go_frame);
    let mut rs_member = member_from(&rs_frame);
    for m in [&mut go_member, &mut rs_member] {
        let map = m.as_object_mut().unwrap();
        map.insert("group_id".into(), "<masked>".into());
        map.insert("create_at".into(), "<masked>".into());
    }
    assert_eq!(go_member, rs_member);
    assert_eq!(rs_member["user_id"], me);
    assert_eq!(
        rs_frame["broadcast"]["user_id"], me,
        "addressed to the member"
    );
    assert_eq!(go_frame["broadcast"]["user_id"], me);

    // Remove them: `group_member_deleted` — note the past tense, unlike `group_member_add`.
    let (s1, _, _) = send(
        &client,
        &pair.go,
        &admin,
        reqwest::Method::DELETE,
        &format!("/api/v4/groups/{go_id}/members"),
        &body,
    )
    .await;
    let (s2, _, _) = send(
        &client,
        &pair.rust,
        &admin,
        reqwest::Method::DELETE,
        &format!("/api/v4/groups/{rs_id}/members"),
        &body,
    )
    .await;
    assert_eq!((s1, s2), (200, 200));
    let removed = |v: &serde_json::Value, id: &str| {
        v["event"] == "group_member_deleted"
            && v["data"]["group_member"]
                .as_str()
                .is_some_and(|g| g.contains(id))
    };
    assert!(
        go_probe
            .collect_until(window, |f| f.iter().any(|v| removed(v, &go_id)))
            .await
    );
    assert!(
        rs_probe
            .collect_until(window, |f| f.iter().any(|v| removed(v, &rs_id)))
            .await
    );
    let rs_frame = rs_probe
        .events_named("group_member_deleted")
        .into_iter()
        .find(|v| removed(v, &rs_id))
        .unwrap();
    assert!(member_from(&rs_frame)["delete_at"].as_i64().unwrap() > 0);

    // Delete and restore each publish `received_group` with the group's new state.
    for (base, probe, id) in [
        (&pair.go, &mut go_probe, &go_id),
        (&pair.rust, &mut rs_probe, &rs_id),
    ] {
        let before = probe.events_named("received_group").len();
        let (s, _, _) = send(
            &client,
            base,
            &admin,
            reqwest::Method::DELETE,
            &format!("/api/v4/groups/{id}"),
            "",
        )
        .await;
        assert_eq!(s, 200);
        assert!(
            probe
                .collect_until(window, |f| f.iter().filter(|v| names(v, id)).count()
                    > before)
                .await
        );
        let frame = probe
            .events_named("received_group")
            .into_iter()
            .rfind(|v| names(v, id))
            .unwrap();
        assert!(
            group_from(&frame)["delete_at"].as_i64().unwrap() > 0,
            "the deleted state"
        );
        let (s, _, _) = send(
            &client,
            base,
            &admin,
            reqwest::Method::POST,
            &format!("/api/v4/groups/{id}/restore"),
            "",
        )
        .await;
        assert_eq!(s, 200);
        assert!(
            probe
                .collect_until(window, |f| f.iter().filter(|v| names(v, id)).count()
                    > before + 1)
                .await
        );
        let frame = probe
            .events_named("received_group")
            .into_iter()
            .rfind(|v| names(v, id))
            .unwrap();
        assert_eq!(group_from(&frame)["delete_at"], 0, "the restored state");
    }

    purge().await;
}
