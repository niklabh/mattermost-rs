//! Cross-server parity for the four writes of `api4/properties.go` on the `boards` group —
//! `POST …/{object_type}/fields`, `PATCH …/fields/{field_id}`, `PATCH …/values/{target_id}` and
//! `PATCH …/system/values`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity properties_writes
//! ```
//!
//! # Each server writes its own rows, into one table
//!
//! A write cannot be sent to both servers and compared byte for byte: each creates a row of
//! its own, with its own id and timestamps — and both rows land in the same `PropertyFields`,
//! where the name-conflict check would refuse the second create of one name on one target. So
//! a field is created under a per-server name (`… go` / `… rs`), and a 2xx is compared
//! **modulo** `id`, `create_at`, `update_at` and `name` (and `field_id` on a value, which names
//! the server's own field); the refusals — which write nothing — byte for byte up to the known
//! gaps. Every row this suite makes carries the `mmrspw` name prefix and is removed first and
//! last.
//!
//! `boards` is the group because it carries no hook ([`mm_app::properties`]), so the writes
//! are ordinary writes on any edition; `channel` is the object type because a channel is the
//! one target the admin can make and a plain user can be inside or outside of.

use crate::common;

use common::{
    GO, RUST, a_team_and_channel_the_user_is_in, assert_error_bodies_match_except_known_gaps,
    client, create_plain_user, fixture_pool, go_minted_token, request_raw, stack_enabled,
};

const PREFIX: &str = "mmrspw";

/// Remove the fields and values this suite wrote, keeping the fixture's channels. Called at
/// both ends of every test that writes, under [`common::PROPERTY_ROWS`]: the sibling
/// `properties` suite counts the rows of a group and must never see these.
async fn purge_fields() {
    let Some(pool) = fixture_pool().await else {
        return;
    };
    for statement in [
        "DELETE FROM propertyvalues WHERE fieldid IN (SELECT id FROM propertyfields WHERE name LIKE 'mmrspw%')",
        "DELETE FROM propertyfields WHERE name LIKE 'mmrspw%'",
    ] {
        let _ = sqlx::query(statement).execute(&pool).await;
    }
}

async fn purge_rows() {
    let Some(pool) = fixture_pool().await else {
        return;
    };
    for statement in [
        "DELETE FROM propertyvalues WHERE fieldid IN (SELECT id FROM propertyfields WHERE name LIKE 'mmrspw%')",
        "DELETE FROM propertyfields WHERE name LIKE 'mmrspw%'",
        "DELETE FROM channelmembers WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrspw%')",
        "DELETE FROM channelmemberhistory WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrspw%')",
        "DELETE FROM sidebarchannels WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrspw%')",
        "DELETE FROM posts WHERE channelid IN (SELECT id FROM channels WHERE name LIKE 'mmrspw%')",
        "DELETE FROM publicchannels WHERE name LIKE 'mmrspw%'",
        "DELETE FROM channels WHERE name LIKE 'mmrspw%'",
    ] {
        let _ = sqlx::query(statement).execute(&pool).await;
    }
}

async fn go_post(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let response = client
        .post(format!("{GO}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "POST {path} failed: {}",
        response.text().await.unwrap_or_default()
    );
    response.json().await.expect("the response decodes")
}

/// `(go, rust)` as `(status, body)`, the Rust side asserted served here.
async fn both(
    client: &reqwest::Client,
    method: reqwest::Method,
    token: &str,
    path: &str,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let (go_status, go_body, _) =
        request_raw(client, GO, method.clone(), Some(token), path, Some(body)).await;
    let (rs_status, rs_body, served) =
        request_raw(client, RUST, method, Some(token), path, Some(body)).await;
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "{path} was not served here"
    );
    ((go_status, go_body), (rs_status, rs_body))
}

fn decode(body: &[u8], context: &str) -> serde_json::Value {
    serde_json::from_slice(body).unwrap_or_else(|e| {
        panic!(
            "{context}: not JSON ({e}): {}",
            String::from_utf8_lossy(body)
        )
    })
}

/// Strip the per-server keys from an object or from every object of an array.
fn stable(value: &serde_json::Value, volatile: &[&str]) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .filter(|(k, _)| !volatile.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|v| stable(v, volatile)).collect())
        }
        other => other.clone(),
    }
}

/// A 2xx on both, bodies equal modulo the volatile keys; Go's and Rust's decoded bodies back.
/// `body` is built per server (`"go"` / `"rs"`), so a created field can carry its own name.
#[allow(clippy::too_many_arguments)]
async fn both_write(
    client: &reqwest::Client,
    method: reqwest::Method,
    token: &str,
    path: &str,
    body: &dyn Fn(&str) -> serde_json::Value,
    expected: u16,
    volatile: &[&str],
    context: &str,
) -> (serde_json::Value, serde_json::Value) {
    let (go_status, go, _) = request_raw(
        client,
        GO,
        method.clone(),
        Some(token),
        path,
        Some(body("go").to_string().as_bytes()),
    )
    .await;
    let (rs_status, rs, served) = request_raw(
        client,
        RUST,
        method,
        Some(token),
        path,
        Some(body("rs").to_string().as_bytes()),
    )
    .await;
    assert_eq!(
        served.as_deref(),
        Some("rust"),
        "{path} was not served here"
    );
    assert_eq!(
        go_status,
        expected,
        "{context}: Go: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        expected,
        "{context}: Rust: {}",
        String::from_utf8_lossy(&rs)
    );
    assert_eq!(go.last(), Some(&b'\n'), "{context}: the encoder's newline");
    assert_eq!(rs.last(), Some(&b'\n'), "{context}: the encoder's newline");
    let go = decode(&go, context);
    let rs = decode(&rs, context);
    assert_eq!(stable(&go, volatile), stable(&rs, volatile), "{context}");
    (go, rs)
}

async fn both_refuse(
    client: &reqwest::Client,
    method: reqwest::Method,
    token: &str,
    path: &str,
    body: &[u8],
    expected: u16,
    context: &str,
) -> serde_json::Value {
    let ((go_status, go), (rs_status, rs)) = both(client, method, token, path, body).await;
    assert_eq!(
        go_status,
        expected,
        "{context}: Go: {}",
        String::from_utf8_lossy(&go)
    );
    assert_eq!(
        rs_status,
        expected,
        "{context}: Rust: {}",
        String::from_utf8_lossy(&rs)
    );
    assert_error_bodies_match_except_known_gaps(&go, &rs, path);
    decode(&go, context)
}

/// `(context, method, path, body, status, error id)` — one create refusal.
type CreateCase<'a> = (&'a str, reqwest::Method, String, Vec<u8>, u16, &'a str);
/// `(context, token, path, body, status, error id)` — one value refusal.
type ValueCase<'a> = (&'a str, &'a str, String, Vec<u8>, u16, &'a str);

const FIELD_VOLATILE: &[&str] = &["id", "create_at", "update_at", "name"];
const VALUE_VOLATILE: &[&str] = &["id", "create_at", "update_at", "field_id"];

struct Fixture {
    /// A public channel the plain user is in.
    channel: String,
    /// A public channel the plain user is not in.
    other: String,
    plain_token: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn fixture(client: &reqwest::Client, admin: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_rows().await;
            let (team, _) = a_team_and_channel_the_user_is_in(client, admin).await;
            let plain = create_plain_user(client, admin, &team, "propwrites").await;
            let make = |tag: &'static str| {
                let team = team.clone();
                async move {
                    go_post(
                        client,
                        admin,
                        "/api/v4/channels",
                        serde_json::json!({
                            "team_id": team,
                            "name": format!("{PREFIX}-{tag}"),
                            "display_name": format!("mmrs property writes {tag}"),
                            "type": "O",
                        }),
                    )
                    .await["id"]
                        .as_str()
                        .expect("a channel id")
                        .to_owned()
                }
            };
            let channel = make("in").await;
            let other = make("out").await;
            go_post(
                client,
                admin,
                &format!("/api/v4/channels/{channel}/members"),
                serde_json::json!({ "user_id": plain.id }),
            )
            .await;
            Fixture {
                channel,
                other,
                plain_token: plain.token,
            }
        })
        .await
}

fn fields_path() -> &'static str {
    "/api/v4/properties/groups/boards/channel/fields"
}

fn field_body(name: &str, target_id: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "type": "text",
        "target_type": "channel",
        "target_id": target_id,
    })
}

// ---------------------------------------------------------------------------------------------

/// The admin creates a field with its three levels nil-filled to `member`; the plain user, a
/// member of the channel, creates one with the levels **pinned** to `member` whatever the body
/// says. The created field carries the URL's object type and the group's id.
#[tokio::test]
async fn a_field_is_created_on_both_with_the_levels_go_assigns() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _rows = common::PROPERTY_ROWS.lock().await;
    let fx = fixture(&client, &admin).await;
    purge_fields().await;

    let (go, _) = both_write(
        &client,
        reqwest::Method::POST,
        &admin,
        fields_path(),
        &|server| field_body(&format!("{PREFIX} admin <&> one {server}"), &fx.channel),
        201,
        FIELD_VOLATILE,
        "admin create",
    )
    .await;
    assert_eq!(go["object_type"], "channel");
    assert_eq!(go["permission_field"], "member");
    assert_eq!(go["name"], format!("{PREFIX} admin <&> one go"));

    let pinned = |server: &str| {
        let mut body = field_body(&format!("{PREFIX} plain two {server}"), &fx.channel);
        body["permission_field"] = serde_json::Value::String("sysadmin".to_owned());
        body
    };
    let (go, _) = both_write(
        &client,
        reqwest::Method::POST,
        &fx.plain_token,
        fields_path(),
        &pinned,
        201,
        FIELD_VOLATILE,
        "plain create",
    )
    .await;
    assert_eq!(
        go["permission_field"], "member",
        "a non-admin's levels are pinned"
    );

    // An admin's explicit level survives.
    let explicit = |server: &str| {
        let mut body = field_body(&format!("{PREFIX} admin three {server}"), &fx.channel);
        body["permission_field"] = serde_json::Value::String("sysadmin".to_owned());
        body
    };
    let (go, _) = both_write(
        &client,
        reqwest::Method::POST,
        &admin,
        fields_path(),
        &explicit,
        201,
        FIELD_VOLATILE,
        "admin explicit level",
    )
    .await;
    assert_eq!(go["permission_field"], "sysadmin");
    assert_eq!(go["permission_values"], "member");
    purge_fields().await;
}

/// The create refusals, in the handler's order: the body, `protected`, the template arm, the
/// target-type switch, and the channel the plain user is not in.
#[tokio::test]
async fn the_create_refusals() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _rows = common::PROPERTY_ROWS.lock().await;
    let fx = fixture(&client, &admin).await;
    purge_fields().await;

    let cases: Vec<CreateCase> = vec![
        ("null body", reqwest::Method::POST, fields_path().to_owned(), b"null".to_vec(), 400, "api.context.invalid_body_param.app_error"),
        ("array body", reqwest::Method::POST, fields_path().to_owned(), b"[]".to_vec(), 400, "api.context.invalid_body_param.app_error"),
        ("protected", reqwest::Method::POST, fields_path().to_owned(),
            serde_json::json!({"name": "mmrspw p", "type": "text", "target_type": "channel", "target_id": fx.channel, "protected": true}).to_string().into_bytes(),
            400, "api.property_field.create.protected_via_api.app_error"),
        ("team without target", reqwest::Method::POST, fields_path().to_owned(),
            serde_json::json!({"name": "mmrspw t", "type": "text", "target_type": "team"}).to_string().into_bytes(),
            400, "api.property_field.create.target_id_required.app_error"),
        ("bogus target type", reqwest::Method::POST, fields_path().to_owned(),
            serde_json::json!({"name": "mmrspw b", "type": "text", "target_type": "bogus"}).to_string().into_bytes(),
            400, "api.property_field.create.invalid_target_type.app_error"),
        ("bad object type", reqwest::Method::POST, "/api/v4/properties/groups/boards/bogus/fields".to_owned(), b"{}".to_vec(), 400, "api.context.invalid_url_param.app_error"),
        ("unknown group", reqwest::Method::POST, "/api/v4/properties/groups/mmrsnogroup/channel/fields".to_owned(), b"{}".to_vec(), 404, "app.property_group.get.app_error"),
    ];
    for (context, method, path, body, status, id) in cases {
        let err = both_refuse(&client, method, &admin, &path, &body, status, context).await;
        assert_eq!(err["id"], id, "{context}");
    }

    // The plain user: a template field wants manage_system; a system-object field is
    // canonicalised to the system target and wants it too; a channel they are not in wants
    // create_post there.
    let err = both_refuse(
        &client,
        reqwest::Method::POST,
        &fx.plain_token,
        "/api/v4/properties/groups/boards/template/fields",
        serde_json::json!({"name": "mmrspw tpl", "type": "text"})
            .to_string()
            .as_bytes(),
        403,
        "plain template",
    )
    .await;
    assert_eq!(err["id"], "api.context.permissions.app_error");
    let err = both_refuse(
        &client,
        reqwest::Method::POST,
        &fx.plain_token,
        "/api/v4/properties/groups/boards/system/fields",
        serde_json::json!({"name": "mmrspw sys", "type": "text", "target_type": "channel", "target_id": fx.channel}).to_string().as_bytes(),
        403,
        "plain system",
    )
    .await;
    assert_eq!(err["id"], "api.context.permissions.app_error");
    let err = both_refuse(
        &client,
        reqwest::Method::POST,
        &fx.plain_token,
        fields_path(),
        field_body("mmrspw out", &fx.other).to_string().as_bytes(),
        403,
        "plain, channel not in",
    )
    .await;
    assert_eq!(err["id"], "api.context.permissions.app_error");
    purge_fields().await;
}

/// Each server patches its own field: a rename is a 200 modulo the volatile keys; the URL's
/// object type must match; an empty name is the patch's own 400; a sysadmin-level field is
/// the plain user's 403, and an options-only patch on a text field collapses to that same
/// full-edit refusal.
#[tokio::test]
async fn a_field_is_patched_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _rows = common::PROPERTY_ROWS.lock().await;
    let fx = fixture(&client, &admin).await;
    purge_fields().await;

    let (go, rs) = both_write(
        &client,
        reqwest::Method::POST,
        &admin,
        fields_path(),
        &|server| field_body(&format!("{PREFIX} patch me {server}"), &fx.channel),
        201,
        FIELD_VOLATILE,
        "create for patch",
    )
    .await;
    let go_id = go["id"].as_str().unwrap().to_owned();
    let rs_id = rs["id"].as_str().unwrap().to_owned();

    let patch = |id: &str| format!("/api/v4/properties/groups/boards/channel/fields/{id}");
    let body =
        |server: &str| serde_json::json!({ "name": format!("  {PREFIX} renamed {server}  ") });
    let (go_status, go_body, _) = request_raw(
        &client,
        GO,
        reqwest::Method::PATCH,
        Some(&admin),
        &patch(&go_id),
        Some(body("go").to_string().as_bytes()),
    )
    .await;
    let (rs_status, rs_body, served) = request_raw(
        &client,
        RUST,
        reqwest::Method::PATCH,
        Some(&admin),
        &patch(&rs_id),
        Some(body("rs").to_string().as_bytes()),
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    let go_patched = decode(&go_body, "go patch");
    let rs_patched = decode(&rs_body, "rs patch");
    assert_eq!(
        stable(&go_patched, FIELD_VOLATILE),
        stable(&rs_patched, FIELD_VOLATILE)
    );
    assert_eq!(
        go_patched["name"],
        format!("{PREFIX} renamed go"),
        "the name is trimmed"
    );
    assert_eq!(go_patched["updated_by"], go["updated_by"]);

    // Refusals on the server's own id, compared as error bodies.
    for (context, object_type, body, status, id) in [
        (
            "object type mismatch",
            "post",
            br#"{"name":"mmrspw x"}"#.to_vec(),
            404,
            "api.property_field.object_type_mismatch.app_error",
        ),
        (
            "empty name",
            "channel",
            br#"{"name":"   "}"#.to_vec(),
            400,
            "model.property_field.is_valid.app_error",
        ),
        (
            "null patch",
            "channel",
            b"null".to_vec(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
    ] {
        let path_go = format!("/api/v4/properties/groups/boards/{object_type}/fields/{go_id}");
        let path_rs = format!("/api/v4/properties/groups/boards/{object_type}/fields/{rs_id}");
        let (go_status, go_body, _) = request_raw(
            &client,
            GO,
            reqwest::Method::PATCH,
            Some(&admin),
            &path_go,
            Some(&body),
        )
        .await;
        let (rs_status, rs_body, served) = request_raw(
            &client,
            RUST,
            reqwest::Method::PATCH,
            Some(&admin),
            &path_rs,
            Some(&body),
        )
        .await;
        assert_eq!(served.as_deref(), Some("rust"), "{context}");
        assert_eq!(
            go_status,
            status,
            "{context}: Go: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            status,
            "{context}: Rust: {}",
            String::from_utf8_lossy(&rs_body)
        );
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &path_go);
        let err = decode(&go_body, context);
        assert_eq!(err["id"], id, "{context}");
    }

    // An unknown field id is the same 404 on both, so it compares directly.
    let err = both_refuse(
        &client,
        reqwest::Method::PATCH,
        &admin,
        &patch("abcdefghijklmnopqrstuvwxyz"),
        br#"{"name":"mmrspw x"}"#,
        404,
        "unknown field",
    )
    .await;
    assert_eq!(err["id"], "app.property.not_found.app_error");

    // A sysadmin-level field: the plain user is refused, an options-only patch included
    // (text does not support options, so it is a full edit).
    let locked = |server: &str| {
        let mut body = field_body(&format!("{PREFIX} locked {server}"), &fx.channel);
        body["permission_field"] = serde_json::Value::String("sysadmin".to_owned());
        body
    };
    let (go, rs) = both_write(
        &client,
        reqwest::Method::POST,
        &admin,
        fields_path(),
        &locked,
        201,
        FIELD_VOLATILE,
        "locked create",
    )
    .await;
    let go_id = go["id"].as_str().unwrap().to_owned();
    let rs_id = rs["id"].as_str().unwrap().to_owned();
    for (context, body) in [
        ("plain full edit", br#"{"name":"mmrspw nope"}"#.to_vec()),
        (
            "plain options-only on text",
            br#"{"attrs":{"options":[]}}"#.to_vec(),
        ),
    ] {
        let (go_status, go_body, _) = request_raw(
            &client,
            GO,
            reqwest::Method::PATCH,
            Some(&fx.plain_token),
            &patch(&go_id),
            Some(&body),
        )
        .await;
        let (rs_status, rs_body, _) = request_raw(
            &client,
            RUST,
            reqwest::Method::PATCH,
            Some(&fx.plain_token),
            &patch(&rs_id),
            Some(&body),
        )
        .await;
        assert_eq!(
            go_status,
            403,
            "{context}: Go: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(rs_status, 403, "{context}");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &patch(&go_id));
        assert_eq!(
            decode(&go_body, context)["id"],
            "api.property_field.update.no_field_permission.app_error",
            "{context}"
        );
    }
    purge_fields().await;
}

/// Values on each server's own field: an upsert is a 200 modulo the volatile keys, and a second
/// upsert of the same field updates rather than duplicates. The system route needs a system
/// field and the admin.
#[tokio::test]
async fn values_are_patched_on_both() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _rows = common::PROPERTY_ROWS.lock().await;
    let fx = fixture(&client, &admin).await;
    purge_fields().await;

    let (go, rs) = both_write(
        &client,
        reqwest::Method::POST,
        &admin,
        fields_path(),
        &|server| field_body(&format!("{PREFIX} valued {server}"), &fx.channel),
        201,
        FIELD_VOLATILE,
        "create for values",
    )
    .await;
    let go_id = go["id"].as_str().unwrap().to_owned();
    let rs_id = rs["id"].as_str().unwrap().to_owned();
    let values_path = format!(
        "/api/v4/properties/groups/boards/channel/values/{}",
        fx.channel
    );

    for round in ["first", "second"] {
        let body = |id: &str| serde_json::json!([{ "field_id": id, "value": format!("mmrspw <&> {round}") }]);
        let (go_status, go_body, _) = request_raw(
            &client,
            GO,
            reqwest::Method::PATCH,
            Some(&fx.plain_token),
            &values_path,
            Some(body(&go_id).to_string().as_bytes()),
        )
        .await;
        let (rs_status, rs_body, served) = request_raw(
            &client,
            RUST,
            reqwest::Method::PATCH,
            Some(&fx.plain_token),
            &values_path,
            Some(body(&rs_id).to_string().as_bytes()),
        )
        .await;
        assert_eq!(served.as_deref(), Some("rust"));
        assert_eq!(
            go_status,
            200,
            "{round}: Go: {}",
            String::from_utf8_lossy(&go_body)
        );
        assert_eq!(
            rs_status,
            200,
            "{round}: Rust: {}",
            String::from_utf8_lossy(&rs_body)
        );
        let go_values = decode(&go_body, round);
        let rs_values = decode(&rs_body, round);
        assert_eq!(
            stable(&go_values, VALUE_VOLATILE),
            stable(&rs_values, VALUE_VOLATILE),
            "{round}"
        );
        assert_eq!(go_values[0]["value"], format!("mmrspw <&> {round}"));
        assert_eq!(go_values[0]["target_type"], "channel");
    }

    // The system route: a system-object field, written by the admin.
    let (go, rs) = both_write(
        &client,
        reqwest::Method::POST,
        &admin,
        "/api/v4/properties/groups/boards/system/fields",
        &|server| serde_json::json!({"name": format!("{PREFIX} system {server}"), "type": "text"}),
        201,
        FIELD_VOLATILE,
        "system field",
    )
    .await;
    assert_eq!(go["target_type"], "system");
    let go_sys = go["id"].as_str().unwrap().to_owned();
    let rs_sys = rs["id"].as_str().unwrap().to_owned();
    let system_path = "/api/v4/properties/groups/boards/system/values";
    let body = |id: &str| serde_json::json!([{ "field_id": id, "value": 42 }]);
    let (go_status, go_body, _) = request_raw(
        &client,
        GO,
        reqwest::Method::PATCH,
        Some(&admin),
        system_path,
        Some(body(&go_sys).to_string().as_bytes()),
    )
    .await;
    let (rs_status, rs_body, served) = request_raw(
        &client,
        RUST,
        reqwest::Method::PATCH,
        Some(&admin),
        system_path,
        Some(body(&rs_sys).to_string().as_bytes()),
    )
    .await;
    assert_eq!(served.as_deref(), Some("rust"));
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    let go_values = decode(&go_body, "system values");
    assert_eq!(
        stable(&go_values, VALUE_VOLATILE),
        stable(&decode(&rs_body, "system values"), VALUE_VOLATILE)
    );
    assert_eq!(go_values[0]["target_id"], "system");
    assert_eq!(go_values[0]["value"], 42);

    // A field of one object type cannot take values under another: the same 404 on both.
    let (go_status, go_body, _) = request_raw(
        &client,
        GO,
        reqwest::Method::PATCH,
        Some(&admin),
        &values_path,
        Some(body(&go_sys).to_string().as_bytes()),
    )
    .await;
    let (rs_status, rs_body, _) = request_raw(
        &client,
        RUST,
        reqwest::Method::PATCH,
        Some(&admin),
        &values_path,
        Some(body(&rs_sys).to_string().as_bytes()),
    )
    .await;
    assert_eq!(go_status, 404, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 404, "{}", String::from_utf8_lossy(&rs_body));
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &values_path);
    assert_eq!(
        decode(&go_body, "mismatch")["id"],
        "api.property_field.object_type_mismatch.app_error"
    );
    purge_fields().await;
}

/// The value refusals that write nothing, compared directly: the two object-type refusals
/// ahead of the target id, the body shapes, the batch limits, the id checks, the plain user
/// outside the channel and on the system route, and an unknown field.
#[tokio::test]
async fn the_value_refusals() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let _rows = common::PROPERTY_ROWS.lock().await;
    let fx = fixture(&client, &admin).await;
    purge_fields().await;
    let values_path = format!(
        "/api/v4/properties/groups/boards/channel/values/{}",
        fx.channel
    );
    let unknown = "abcdefghijklmnopqrstuvwxyz";
    let fifty_one: Vec<serde_json::Value> = (0..51)
        .map(|i| serde_json::json!({ "field_id": format!("{unknown:0>25}{}", i % 10), "value": 1 }))
        .collect();

    let cases: Vec<ValueCase> = vec![
        (
            "template",
            &admin,
            "/api/v4/properties/groups/boards/template/values/abc".to_owned(),
            b"[]".to_vec(),
            400,
            "api.property_value.template_no_values.app_error",
        ),
        (
            "system on the generic route",
            &admin,
            "/api/v4/properties/groups/boards/system/values/abc".to_owned(),
            b"[]".to_vec(),
            400,
            "api.property_value.system_use_dedicated_route.app_error",
        ),
        (
            "short target",
            &admin,
            "/api/v4/properties/groups/boards/channel/values/abc".to_owned(),
            b"[]".to_vec(),
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "object body",
            &admin,
            values_path.clone(),
            b"{}".to_vec(),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "null body",
            &admin,
            values_path.clone(),
            b"null".to_vec(),
            400,
            "api.property_value.patch.empty_body.app_error",
        ),
        (
            "empty batch",
            &admin,
            values_path.clone(),
            b"[]".to_vec(),
            400,
            "api.property_value.patch.empty_body.app_error",
        ),
        (
            "too many",
            &admin,
            values_path.clone(),
            serde_json::Value::Array(fifty_one).to_string().into_bytes(),
            400,
            "api.property_value.patch.too_many_items.request_error",
        ),
        (
            "short field id",
            &admin,
            values_path.clone(),
            br#"[{"field_id":"abc","value":1}]"#.to_vec(),
            400,
            "api.property_value.patch.invalid_field_id.app_error",
        ),
        (
            "duplicate field id",
            &admin,
            values_path.clone(),
            format!(
                r#"[{{"field_id":"{unknown}","value":1}},{{"field_id":"{unknown}","value":2}}]"#
            )
            .into_bytes(),
            400,
            "api.property_value.patch.duplicate_field_id.app_error",
        ),
        (
            "plain outside the channel",
            &fx.plain_token,
            format!(
                "/api/v4/properties/groups/boards/channel/values/{}",
                fx.other
            ),
            b"[]".to_vec(),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "plain on the system route",
            &fx.plain_token,
            "/api/v4/properties/groups/boards/system/values".to_owned(),
            b"[]".to_vec(),
            403,
            "api.context.permissions.app_error",
        ),
        (
            "missing channel",
            &admin,
            format!("/api/v4/properties/groups/boards/channel/values/{unknown}"),
            b"[]".to_vec(),
            404,
            "app.channel.get.existing.app_error",
        ),
    ];
    for (context, token, path, body, status, id) in cases {
        let err = both_refuse(
            &client,
            reqwest::Method::PATCH,
            token,
            &path,
            &body,
            status,
            context,
        )
        .await;
        assert_eq!(err["id"], id, "{context}");
    }

    // An unknown field id past the checks: whatever GetPropertyFields answers, the same on both.
    let ((go_status, go_body), (rs_status, rs_body)) = both(
        &client,
        reqwest::Method::PATCH,
        &admin,
        &values_path,
        format!(r#"[{{"field_id":"{unknown}","value":1}}]"#).as_bytes(),
    )
    .await;
    assert_eq!(
        go_status,
        rs_status,
        "unknown field: Go {} / Rust {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    assert!(go_status >= 400);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &values_path);

    purge_fields().await;
}
