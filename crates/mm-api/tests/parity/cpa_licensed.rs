//! Cross-server parity for the seven custom-profile-attribute routes on the **licensed pair** —
//! the enterprise-ready Go oracle with the stack's Enterprise licence, and an mm-api carrying
//! the same licence (`common::licensed`).
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity cpa_licensed
//! ```
//!
//! # What this suite adds to `custom_profile_attributes`
//!
//! That suite measures the unlicensed contract: five different refusals across seven routes.
//! This one measures what a licence unlocks — the success path of every write, the hook chain
//! behind it, the read filtering, and the four CPA websocket events. Until 2026-09-13 none of it
//! had a Go answer on this stack ([D-300]).
//!
//! # Every write goes to both servers, and both write the shared table
//!
//! A create through the Go oracle and a create through the Rust server each insert a row, with
//! their own id and timestamps. So a write is compared **scrubbed** — id, `create_at`,
//! `update_at` and (where the two must differ) `name` removed — and the *state* is compared
//! exactly afterwards, by reading the same table through both servers. Values carry no id in
//! their responses and are compared byte for byte.
//!
//! # Every test holds `PROPERTY_ROWS` exclusively
//!
//! A field in the `access_control` group is global state: the unlicensed suites assert
//! `200 []` against an empty group, and a field created here would flip them to 403. So each
//! test creates under one name prefix, purges before and after, and holds the lock the whole
//! time. `ACTIVE_LICENCE_ROW` is held shared — the licence here is the pair's environment, never
//! a row.

use std::time::Duration;

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, LicensedPair, PROPERTY_ROWS, SocketProbe,
    assert_error_bodies_match_except_known_gaps, client, create_plain_user, create_team,
    delete_plain_user, go_minted_token, licensed, logged_in_user_id, stack_enabled,
};

/// Every field this suite creates or plants carries this in its name or id.
const PREFIX: &str = "mmrscpalic";
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// A 26-character id under the suite's prefix — padded with zeros, and **checked**: a planted
/// row with a 27-character id is refused by the column and takes the whole test with it.
fn planted_id(suffix: &str) -> String {
    let id = format!("{PREFIX}{suffix:0>16}");
    assert_eq!(id.len(), 26, "{id}");
    id
}
const FIELDS: &str = "/api/v4/custom_profile_attributes/fields";
const VALUES: &str = "/api/v4/custom_profile_attributes/values";

fn field_path(id: &str) -> String {
    format!("{FIELDS}/{id}")
}
fn user_values_path(user_id: &str) -> String {
    format!("/api/v4/users/{user_id}/custom_profile_attributes")
}

/// One request to one server of the pair.
async fn send(
    client: &reqwest::Client,
    base: &str,
    ours: bool,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<&str>,
) -> (u16, Vec<u8>) {
    let mut request = client
        .request(method, format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"));
    if let Some(body) = body {
        request = request
            .header("Content-Type", "application/json")
            .body(body.to_owned());
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    if ours {
        common::assert_served_by_rust(response.headers(), path);
    }
    (status, response.bytes().await.expect("body reads").to_vec())
}

/// The same request to both licensed servers: `(go, rust)`.
async fn both(
    client: &reqwest::Client,
    pair: &LicensedPair,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<&str>,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    (
        send(client, &pair.go, false, token, method.clone(), path, body).await,
        send(client, &pair.rust, true, token, method, path, body).await,
    )
}

fn same_error(go: &(u16, Vec<u8>), rs: &(u16, Vec<u8>), status: u16, id: &str, context: &str) {
    assert_eq!(
        go.0,
        status,
        "{context}: Go's status ({})",
        String::from_utf8_lossy(&go.1)
    );
    assert_eq!(
        rs.0,
        status,
        "{context}: our status ({})",
        String::from_utf8_lossy(&rs.1)
    );
    let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, context);
    assert_eq!(body["id"].as_str(), Some(id), "{context}: the error id");
}

fn scrub(value: &mut serde_json::Value, scrub_name: bool) {
    if let Some(object) = value.as_object_mut() {
        for key in ["id", "create_at", "update_at"] {
            object.remove(key);
        }
        if scrub_name {
            object.remove("name");
        }
    }
}

async fn purge() {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    // By prefix, and by authorship: a field this suite created through the API and then failed
    // to rename carries the admin's id in `createdby`, which no planted row does.
    let filter = "name LIKE $1 OR id LIKE $1 OR (createdby <> '' AND objecttype = 'user' AND groupid = (SELECT id FROM propertygroups WHERE name = 'access_control'))";
    let _ = sqlx::query(&format!(
        "DELETE FROM propertyvalues WHERE fieldid IN (SELECT id FROM propertyfields WHERE {filter})"
    ))
    .bind(format!("{PREFIX}%"))
    .execute(&pool)
    .await;
    let _ = sqlx::query(&format!("DELETE FROM propertyfields WHERE {filter}"))
        .bind(format!("{PREFIX}%"))
        .execute(&pool)
        .await;
}

async fn access_control_group_id() -> String {
    let pool = common::fixture_pool().await.expect("the fixture pool");
    sqlx::query_scalar::<_, String>("SELECT id FROM propertygroups WHERE name = 'access_control'")
        .fetch_one(&pool)
        .await
        .expect("the access_control group exists")
}

/// Plant a `user` field with arbitrary attrs — the only way to get `protected`, an
/// `access_mode`, `owners` or a sync source onto a field, since the REST API refuses all four.
async fn plant_field(id: &str, name: &str, type_: &str, attrs: &str) {
    let pool = common::fixture_pool().await.expect("the fixture pool");
    let group = access_control_group_id().await;
    sqlx::query(
        "INSERT INTO propertyfields
            (id, groupid, name, type, attrs, targetid, targettype, objecttype, protected,
             permissionfield, permissionvalues, permissionoptions,
             createat, updateat, deleteat, createdby, updatedby)
         VALUES ($1, $2, $3, $4::text::property_field_type, $5::jsonb, '', 'system', 'user',
                 false, 'sysadmin', 'member', 'sysadmin',
                 1788600000000, 1788600000000, 0, '', '')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(&group)
    .bind(name)
    .bind(type_)
    .bind(attrs)
    .execute(&pool)
    .await
    .expect("the planted field is written");
}

async fn plant_value(id: &str, field_id: &str, user_id: &str, value: &str) {
    let pool = common::fixture_pool().await.expect("the fixture pool");
    let group = access_control_group_id().await;
    sqlx::query(
        "INSERT INTO propertyvalues
            (id, targetid, targettype, groupid, fieldid, value, createat, updateat, deleteat)
         VALUES ($1, $2, 'user', $3, $4, $5::jsonb, 1788600000000, 1788600000000, 0)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(user_id)
    .bind(&group)
    .bind(field_id)
    .bind(value)
    .execute(&pool)
    .await
    .expect("the planted value is written");
}

fn text_field(name: &str) -> String {
    format!(
        r#"{{"name":"{name}","type":"text","attrs":{{"visibility":"always","sort_order":3,"value_type":"email","display_name":"Display name"}}}}"#
    )
}

fn select_field(name: &str) -> String {
    format!(
        r##"{{"name":"{name}","type":"select","attrs":{{"visibility":"when_set","sort_order":1,"options":[{{"name":"Apples","color":"#ff0000"}},{{"name":"Bananas","color":""}}]}}}}"##
    )
}

/// Create a field through one server, returning the response object. **201**, and the body
/// carries the encoder's newline.
async fn create_via(
    client: &reqwest::Client,
    base: &str,
    ours: bool,
    token: &str,
    body: &str,
) -> serde_json::Value {
    let (status, bytes) = send(
        client,
        base,
        ours,
        token,
        reqwest::Method::POST,
        FIELDS,
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        201,
        "create via {base}: {}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(
        bytes.ends_with(b"\n"),
        "the create body carries the encoder's newline"
    );
    serde_json::from_slice(&bytes).expect("the created field is JSON")
}

/// `GET /fields` through both servers, which read the same table: identical bytes.
async fn list_agrees(
    client: &reqwest::Client,
    pair: &LicensedPair,
    token: &str,
    context: &str,
) -> serde_json::Value {
    let (go, rs) = both(client, pair, token, reqwest::Method::GET, FIELDS, None).await;
    assert_eq!(go.0, 200, "{context}: {}", String::from_utf8_lossy(&go.1));
    assert_eq!(rs.0, 200, "{context}: {}", String::from_utf8_lossy(&rs.1));
    assert_eq!(
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1),
        "{context}: the field list must be byte-identical"
    );
    serde_json::from_slice(&go.1).expect("the list is JSON")
}

struct Fixture {
    client: reqwest::Client,
    pair: LicensedPair,
    admin: String,
    plain: common::PlainUser,
    team: String,
}

async fn fixture(tag: &str) -> Fixture {
    let client = client();
    let admin = go_minted_token(&client).await;
    let pair = licensed().await;
    let team = create_team(&client, &admin, tag).await;
    let plain = create_plain_user(&client, &admin, &team, tag).await;
    purge().await;
    Fixture {
        client,
        pair,
        admin,
        plain,
        team,
    }
}

impl Fixture {
    async fn teardown(self) {
        purge().await;
        delete_plain_user(&self.client, &self.admin, &self.plain.id).await;
        let _ = self.team;
    }
}

/// The success path of the create, on both servers: the same body produces the same row shape —
/// the attrs sanitised and defaulted the same way, the three permission levels pinned the same
/// way, `created_by` the caller — and the list afterwards is byte-identical through either.
#[tokio::test]
async fn creating_a_field_writes_the_same_shape_through_either_server() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let f = fixture("cpalcreate").await;

    let go = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &text_field(&format!("{PREFIX}_go")),
    )
    .await;
    let rs = create_via(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        &text_field(&format!("{PREFIX}_rs")),
    )
    .await;
    let (mut go_s, mut rs_s) = (go.clone(), rs.clone());
    scrub(&mut go_s, true);
    scrub(&mut rs_s, true);
    assert_eq!(go_s, rs_s, "the created field, scrubbed");
    assert_eq!(rs["permission_field"], "sysadmin");
    assert_eq!(
        rs["permission_values"], "member",
        "a user field defaults to member-writable"
    );
    assert_eq!(rs["permission_options"], "sysadmin");
    assert_eq!(rs["created_by"], logged_in_user_id());
    assert_eq!(rs["attrs"]["visibility"], "always");
    assert_eq!(rs["attrs"]["display_name"], "Display name");
    assert_eq!(rs["target_type"], "system");
    assert_eq!(rs["object_type"], "user");

    // A select field: options get ids minted server-side, and the list sorts by sort_order.
    let go_sel = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &select_field(&format!("{PREFIX}_sgo")),
    )
    .await;
    let rs_sel = create_via(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        &select_field(&format!("{PREFIX}_srs")),
    )
    .await;
    for created in [&go_sel, &rs_sel] {
        let options = created["attrs"]["options"].as_array().expect("options");
        assert_eq!(options.len(), 2);
        for option in options {
            assert_eq!(
                option["id"].as_str().map(str::len),
                Some(26),
                "each option got an id"
            );
        }
        assert!(
            created["attrs"].get("value_type").is_some(),
            "the CPA attrs always carry value_type"
        );
    }

    let list = list_agrees(&f.client, &f.pair, &f.admin, "after four creates").await;
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert_eq!(names.len(), 4);
    assert!(
        names[0].starts_with(&format!("{PREFIX}_s")),
        "sort_order 1 before 3: {names:?}"
    );

    f.teardown().await;
}

/// The refusals a licensed create can produce, each compared: the permission, the CEL name
/// rule, each attribute-hook arm, the access-control arms a human caller trips, and the
/// name conflict.
#[tokio::test]
async fn create_refusals_agree_on_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let f = fixture("cpalcrerr").await;
    let admin = f.admin.as_str();
    let name = |suffix: &str| format!("{PREFIX}_{suffix}");

    let cases: Vec<(&str, String, u16, &str)> = vec![
        (
            "reserved name",
            r#"{"name":"in","type":"text"}"#.to_owned(),
            422,
            "model.cpa_field.name.reserved_word.app_error",
        ),
        (
            "name pattern",
            format!(r#"{{"name":"{PREFIX}-dash","type":"text"}}"#),
            422,
            "model.cpa_field.name.invalid_charset.app_error",
        ),
        (
            "bad visibility",
            format!(
                r#"{{"name":"{}","type":"text","attrs":{{"visibility":"sometimes"}}}}"#,
                name("vis")
            ),
            400,
            "app.property_field.invalid_attrs.app_error",
        ),
        (
            "bad value_type",
            format!(
                r#"{{"name":"{}","type":"text","attrs":{{"value_type":"iban"}}}}"#,
                name("vt")
            ),
            400,
            "app.property_field.invalid_attrs.app_error",
        ),
        (
            "managed not admin",
            format!(
                r#"{{"name":"{}","type":"text","attrs":{{"managed":"team"}}}}"#,
                name("man")
            ),
            400,
            "app.property_field.invalid_attrs.app_error",
        ),
        (
            "select without options",
            format!(
                r#"{{"name":"{}","type":"select","attrs":{{"options":[]}}}}"#,
                name("noopt")
            ),
            400,
            "app.property_field.invalid_attrs.app_error",
        ),
        (
            "duplicate option names",
            format!(
                r#"{{"name":"{}","type":"select","attrs":{{"options":[{{"name":"A"}},{{"name":"A"}}]}}}}"#,
                name("dupopt")
            ),
            400,
            "app.property_field.invalid_attrs.app_error",
        ),
        (
            "rank without ranks",
            format!(
                r#"{{"name":"{}","type":"rank","attrs":{{"options":[{{"name":"A"}},{{"name":"B"}}]}}}}"#,
                name("rank")
            ),
            400,
            "app.property_field.invalid_attrs.app_error",
        ),
        (
            "protected by a human",
            format!(
                r#"{{"name":"{}","type":"text","attrs":{{"protected":true}}}}"#,
                name("prot")
            ),
            403,
            "app.property.access_denied.app_error",
        ),
        (
            "source plugin by a human",
            format!(
                r#"{{"name":"{}","type":"text","attrs":{{"source_plugin_id":"com.example"}}}}"#,
                name("src")
            ),
            403,
            "app.property.access_denied.app_error",
        ),
        (
            "access mode needs protected",
            format!(
                r#"{{"name":"{}","type":"text","attrs":{{"access_mode":"source_only"}}}}"#,
                name("mode")
            ),
            400,
            "app.property.invalid_access_mode.app_error",
        ),
        (
            "unknown access mode",
            format!(
                r#"{{"name":"{}","type":"text","attrs":{{"access_mode":"secret"}}}}"#,
                name("mode2")
            ),
            400,
            "app.property.invalid_access_mode.app_error",
        ),
        (
            "owners with managed",
            format!(
                r#"{{"name":"{}","type":"text","attrs":{{"managed":"admin","owners":[{{"id":"x","type":"plugin"}}]}}}}"#,
                name("own")
            ),
            400,
            "app.property_field.invalid_attrs.app_error",
        ),
        (
            "bad owner type",
            format!(
                r#"{{"name":"{}","type":"text","attrs":{{"owners":[{{"id":"x","type":"robot"}}]}}}}"#,
                name("own2")
            ),
            400,
            "app.property_field.invalid_attrs.app_error",
        ),
        (
            "linked to nothing",
            format!(
                r#"{{"name":"{}","type":"text","linked_field_id":"{NOWHERE}"}}"#,
                name("link")
            ),
            400,
            "app.property_field.create.linked_source_not_found.app_error",
        ),
        (
            "unknown type",
            format!(r#"{{"name":"{}","type":"blob"}}"#, name("typ")),
            400,
            "model.property_field.is_valid.app_error",
        ),
    ];
    for (context, body, status, id) in &cases {
        let (go, rs) = both(
            &f.client,
            &f.pair,
            admin,
            reqwest::Method::POST,
            FIELDS,
            Some(body),
        )
        .await;
        same_error(&go, &rs, *status, id, context);
    }

    // A plain user never learns any of that: the permission comes first.
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.plain.token,
        reqwest::Method::POST,
        FIELDS,
        Some(&text_field(&name("plain"))),
    )
    .await;
    same_error(
        &go,
        &rs,
        403,
        "api.context.permissions.app_error",
        "plain user creating",
    );

    // The name conflict: 409, from the second server to try the name.
    create_via(
        &f.client,
        &f.pair.go,
        false,
        admin,
        &text_field(&name("taken")),
    )
    .await;
    let (go, rs) = both(
        &f.client,
        &f.pair,
        admin,
        reqwest::Method::POST,
        FIELDS,
        Some(&text_field(&name("taken"))),
    )
    .await;
    same_error(
        &go,
        &rs,
        409,
        "app.property_field.create.name_conflict.app_error",
        "duplicate name",
    );

    // `managed: admin` by an administrator is accepted and pins values to sysadmin.
    let go = create_via(
        &f.client,
        &f.pair.go,
        false,
        admin,
        &format!(
            r#"{{"name":"{}","type":"text","attrs":{{"managed":"admin"}}}}"#,
            name("mgo")
        ),
    )
    .await;
    let rs = create_via(
        &f.client,
        &f.pair.rust,
        true,
        admin,
        &format!(
            r#"{{"name":"{}","type":"text","attrs":{{"managed":"admin"}}}}"#,
            name("mrs")
        ),
    )
    .await;
    assert_eq!(go["permission_values"], "sysadmin");
    assert_eq!(rs["permission_values"], "sysadmin");
    assert_eq!(
        rs["attrs"]["visibility"], "when_set",
        "the visibility default was applied"
    );

    // Nothing above the group's user-field cap: the twenty-first user field is a 422.
    // Three so far (`taken`, `mgo`, `mrs`); fill to the cap of twenty.
    for i in 0..(20 - 3) {
        create_via(
            &f.client,
            &f.pair.go,
            false,
            admin,
            &format!(
                r#"{{"name":"{}","type":"text"}}"#,
                name(&format!("fill{i}"))
            ),
        )
        .await;
    }
    let (go, rs) = both(
        &f.client,
        &f.pair,
        admin,
        reqwest::Method::POST,
        FIELDS,
        Some(&text_field(&name("over"))),
    )
    .await;
    same_error(
        &go,
        &rs,
        422,
        "app.property_field.create.limit_reached.app_error",
        "the twenty-first user field",
    );

    f.teardown().await;
}

/// The success path of the patch: a rename, a visibility change and new options through each
/// server, compared scrubbed; then the state through both.
#[tokio::test]
async fn patching_a_field_agrees_through_either_server() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let f = fixture("cpalpatch").await;

    let go = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &select_field(&format!("{PREFIX}_pgo")),
    )
    .await;
    let rs = create_via(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        &select_field(&format!("{PREFIX}_prs")),
    )
    .await;
    let go_id = go["id"].as_str().unwrap();
    let rs_id = rs["id"].as_str().unwrap();

    let patch = |name: &str| {
        format!(
            r##"{{"name":"{name}","attrs":{{"visibility":"hidden","options":[{{"name":"Cherries","color":"#00ff00"}}],"display_name":"  padded  "}}}}"##
        )
    };
    let (go_status, go_body) = send(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        reqwest::Method::PATCH,
        &field_path(go_id),
        Some(&patch(&format!("{PREFIX}_pgo2"))),
    )
    .await;
    let (rs_status, rs_body) = send(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        reqwest::Method::PATCH,
        &field_path(rs_id),
        Some(&patch(&format!("{PREFIX}_prs2"))),
    )
    .await;
    assert_eq!(go_status, 200, "{}", String::from_utf8_lossy(&go_body));
    assert_eq!(rs_status, 200, "{}", String::from_utf8_lossy(&rs_body));
    let mut go_v: serde_json::Value = serde_json::from_slice(&go_body).unwrap();
    let mut rs_v: serde_json::Value = serde_json::from_slice(&rs_body).unwrap();
    // The new option's id is minted per write.
    for v in [&mut go_v, &mut rs_v] {
        scrub(v, true);
        if let Some(options) = v["attrs"]["options"].as_array_mut() {
            for o in options {
                o.as_object_mut().unwrap().remove("id");
            }
        }
    }
    assert_eq!(go_v, rs_v, "the patched field, scrubbed");
    assert_eq!(rs_v["attrs"]["visibility"], "hidden");
    assert_eq!(
        rs_v["attrs"]["display_name"], "padded",
        "the attribute hook trims"
    );
    assert_eq!(
        rs_v["attrs"]["sort_order"], 1,
        "merge: an untouched attr survives"
    );

    list_agrees(&f.client, &f.pair, &f.admin, "after two patches").await;

    // A patch of only `options` on a select field is the options permission, which an admin
    // also holds; the response shape is the same.
    let only = r#"{"attrs":{"options":[{"name":"Dates"}]}}"#;
    let (go2, rs2) = (
        send(
            &f.client,
            &f.pair.go,
            false,
            &f.admin,
            reqwest::Method::PATCH,
            &field_path(go_id),
            Some(only),
        )
        .await,
        send(
            &f.client,
            &f.pair.rust,
            true,
            &f.admin,
            reqwest::Method::PATCH,
            &field_path(rs_id),
            Some(only),
        )
        .await,
    );
    assert_eq!(
        (go2.0, rs2.0),
        (200, 200),
        "{} / {}",
        String::from_utf8_lossy(&go2.1),
        String::from_utf8_lossy(&rs2.1)
    );
    list_agrees(
        &f.client,
        &f.pair,
        &f.admin,
        "after the options-only patches",
    )
    .await;

    f.teardown().await;
}

/// The patch refusals: the two permission ids, the object-type mismatch, the attribute hook,
/// the CEL rule on a rename, and the rename onto a taken name.
#[tokio::test]
async fn patch_refusals_agree_on_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let f = fixture("cpalperr").await;

    let a = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &select_field(&format!("{PREFIX}_a")),
    )
    .await;
    let b = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &text_field(&format!("{PREFIX}_b")),
    )
    .await;
    let a_id = a["id"].as_str().unwrap();
    let channel_id = planted_id("chan1");
    plant_field(&channel_id, &format!("{PREFIX}_chan"), "text", "{}").await;
    // A planted channel-object field, which the CPA routes must not touch.
    let pool = common::fixture_pool().await.unwrap();
    sqlx::query("UPDATE propertyfields SET objecttype = 'channel' WHERE id = $1")
        .bind(&channel_id)
        .execute(&pool)
        .await
        .unwrap();

    let rename_taken = format!(r#"{{"name":"{PREFIX}_b"}}"#);
    let late_link = format!(r#"{{"linked_field_id":"{NOWHERE}"}}"#);
    let cases: Vec<(&str, String, &str, &str, u16, &str)> = vec![
        (
            "unknown id",
            field_path(NOWHERE),
            &f.admin,
            r#"{"name":"x"}"#,
            404,
            "app.property.not_found.app_error",
        ),
        (
            "channel object type",
            field_path(&channel_id),
            &f.admin,
            r#"{"name":"x"}"#,
            404,
            "api.property_field.object_type_mismatch.app_error",
        ),
        (
            "plain user edits",
            field_path(a_id),
            &f.plain.token,
            r#"{"attrs":{"visibility":"hidden"}}"#,
            403,
            "api.property_field.update.no_field_permission.app_error",
        ),
        (
            "plain user options only",
            field_path(a_id),
            &f.plain.token,
            r#"{"attrs":{"options":[{"name":"Z"}]}}"#,
            403,
            "api.property_field.update.no_options_permission.app_error",
        ),
        (
            "bad visibility",
            field_path(a_id),
            &f.admin,
            r#"{"attrs":{"visibility":"sometimes"}}"#,
            400,
            "app.property_field.invalid_attrs.app_error",
        ),
        (
            "rename to reserved",
            field_path(a_id),
            &f.admin,
            r#"{"name":"while"}"#,
            422,
            "model.cpa_field.name.reserved_word.app_error",
        ),
        (
            "rename to taken",
            field_path(a_id),
            &f.admin,
            rename_taken.as_str(),
            409,
            "app.property_field.update.name_conflict.app_error",
        ),
        (
            "empty options on select",
            field_path(a_id),
            &f.admin,
            r#"{"attrs":{"options":[]}}"#,
            400,
            "app.property_field.invalid_attrs.app_error",
        ),
        (
            "late link",
            field_path(a_id),
            &f.admin,
            late_link.as_str(),
            400,
            "app.property_field.update.cannot_link_existing.app_error",
        ),
    ];
    for (context, path, token, body, status, id) in &cases {
        let (go, rs) = both(
            &f.client,
            &f.pair,
            token,
            reqwest::Method::PATCH,
            path,
            Some(body),
        )
        .await;
        same_error(&go, &rs, *status, id, context);
    }
    let _ = b;

    f.teardown().await;
}

/// A type change on a standalone field clears its values (the type-change cleanup hook) and the
/// CPA event says so; select ↔ rank does not.
#[tokio::test]
async fn a_type_change_clears_values_on_both_and_the_event_says_so() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let f = fixture("cpaltype").await;
    let me = logged_in_user_id();

    // One select field per server, each holding a value for the admin.
    let go = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &select_field(&format!("{PREFIX}_tgo")),
    )
    .await;
    let rs = create_via(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        &select_field(&format!("{PREFIX}_trs")),
    )
    .await;
    let go_id = go["id"].as_str().unwrap().to_owned();
    let rs_id = rs["id"].as_str().unwrap().to_owned();
    let go_opt = go["attrs"]["options"][0]["id"].as_str().unwrap().to_owned();
    let rs_opt = rs["attrs"]["options"][0]["id"].as_str().unwrap().to_owned();
    let (s1, b1) = send(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        reqwest::Method::PATCH,
        VALUES,
        Some(&format!(r#"{{"{go_id}":"{go_opt}"}}"#)),
    )
    .await;
    let (s2, b2) = send(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        reqwest::Method::PATCH,
        VALUES,
        Some(&format!(r#"{{"{rs_id}":"{rs_opt}"}}"#)),
    )
    .await;
    assert_eq!(
        (s1, s2),
        (200, 200),
        "{} / {}",
        String::from_utf8_lossy(&b1),
        String::from_utf8_lossy(&b2)
    );

    let mut go_probe = SocketProbe::connect(&f.pair.go, &f.admin).await;
    let mut rs_probe = SocketProbe::connect(&f.pair.rust, &f.admin).await;

    let to_text = r#"{"type":"text"}"#;
    let (s1, b1) = send(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        reqwest::Method::PATCH,
        &field_path(&go_id),
        Some(to_text),
    )
    .await;
    let (s2, b2) = send(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        reqwest::Method::PATCH,
        &field_path(&rs_id),
        Some(to_text),
    )
    .await;
    assert_eq!(
        (s1, s2),
        (200, 200),
        "{} / {}",
        String::from_utf8_lossy(&b1),
        String::from_utf8_lossy(&b2)
    );
    let go_v: serde_json::Value = serde_json::from_slice(&b1).unwrap();
    let rs_v: serde_json::Value = serde_json::from_slice(&b2).unwrap();
    assert_eq!(
        go_v["attrs"]["options"],
        serde_json::Value::Null,
        "a text field carries no options — the hook cleared them, and a nil slice is null"
    );
    assert_eq!(rs_v["attrs"]["options"], go_v["attrs"]["options"]);

    // The values are gone, through both servers.
    let (gv, rv) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::GET,
        &user_values_path(me),
        None,
    )
    .await;
    assert_eq!(gv.0, 200);
    assert_eq!(
        String::from_utf8_lossy(&gv.1),
        String::from_utf8_lossy(&rv.1)
    );
    let values: serde_json::Value = serde_json::from_slice(&gv.1).unwrap();
    assert!(
        values.get(&go_id).is_none() && values.get(&rs_id).is_none(),
        "cleared: {values}"
    );

    // And each server said so on its own socket.
    for (probe, field_id) in [(&mut go_probe, &go_id), (&mut rs_probe, &rs_id)] {
        let found = probe
            .collect_until(Duration::from_secs(5), |frames| {
                frames.iter().any(|fr| {
                    fr["event"] == "custom_profile_attributes_field_updated"
                        && fr["data"]["field"]["id"] == field_id.as_str()
                })
            })
            .await;
        assert!(found, "no field_updated event for {field_id}");
        let updated = probe.events_named("custom_profile_attributes_field_updated");
        let ours = updated
            .iter()
            .find(|fr| fr["data"]["field"]["id"] == field_id.as_str())
            .unwrap();
        assert_eq!(
            ours["data"]["delete_values"], true,
            "the type change cleared values"
        );
        assert!(
            probe
                .events_named("property_values_updated")
                .iter()
                .any(|fr| fr["data"]["field_id"] == field_id.as_str()
                    && fr["data"]["values"] == "[]"),
            "the generic values_updated event for the cleared field"
        );
    }

    f.teardown().await;
}

/// The delete: `{"status":"OK"}` without a newline through either server, a second delete of
/// the same field also succeeds (the row is read without a `DeleteAt` filter), the unknown id is
/// a 404 and the plain user a 403 with the delete's own id.
#[tokio::test]
async fn deleting_a_field_agrees_through_either_server() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let f = fixture("cpaldel").await;

    let a = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &text_field(&format!("{PREFIX}_da")),
    )
    .await;
    let b = create_via(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        &text_field(&format!("{PREFIX}_db")),
    )
    .await;
    let a_id = a["id"].as_str().unwrap();
    let b_id = b["id"].as_str().unwrap();

    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.plain.token,
        reqwest::Method::DELETE,
        &field_path(a_id),
        None,
    )
    .await;
    same_error(
        &go,
        &rs,
        403,
        "api.property_field.delete.no_permission.app_error",
        "plain user deleting",
    );
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::DELETE,
        &field_path(NOWHERE),
        None,
    )
    .await;
    same_error(
        &go,
        &rs,
        404,
        "app.property.not_found.app_error",
        "deleting nothing",
    );

    // Cross-wise: Go's field through us, ours through Go.
    let (rs_status, rs_body) = send(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        reqwest::Method::DELETE,
        &field_path(a_id),
        None,
    )
    .await;
    let (go_status, go_body) = send(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        reqwest::Method::DELETE,
        &field_path(b_id),
        None,
    )
    .await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(
        go_body,
        br#"{"status":"OK"}"#.to_vec(),
        "ReturnStatusOK, no newline"
    );
    assert_eq!(rs_body, go_body);

    let list = list_agrees(&f.client, &f.pair, &f.admin, "after both deletes").await;
    assert_eq!(list, serde_json::json!([]));

    // Again: the soft-deleted row is still found by id, so the second delete is a 200 too.
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::DELETE,
        &field_path(a_id),
        None,
    )
    .await;
    assert_eq!(
        (go.0, rs.0),
        (200, 200),
        "{} / {}",
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1)
    );
    assert_eq!(go.1, rs.1);

    f.teardown().await;
}

/// The value routes on the success path: the same batch through both servers is byte-identical
/// in the response and in every read that follows, for the admin's own values, a plain user's
/// own, and the admin writing a plain user's.
#[tokio::test]
async fn values_round_trip_identically_through_either_server() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let f = fixture("cpalval").await;
    let me = logged_in_user_id();

    let text = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &text_field(&format!("{PREFIX}_vtext")),
    )
    .await;
    let sel = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &select_field(&format!("{PREFIX}_vsel")),
    )
    .await;
    let multi = create_via(&f.client, &f.pair.go, false, &f.admin, &format!(
        r#"{{"name":"{PREFIX}_vmulti","type":"multiselect","attrs":{{"options":[{{"name":"One"}},{{"name":"Two"}},{{"name":"Three"}}]}}}}"#
    )).await;
    let text_id = text["id"].as_str().unwrap();
    let sel_id = sel["id"].as_str().unwrap();
    let opt = sel["attrs"]["options"][1]["id"].as_str().unwrap();
    let multi_id = multi["id"].as_str().unwrap();
    let m1 = multi["attrs"]["options"][0]["id"].as_str().unwrap();
    let m3 = multi["attrs"]["options"][2]["id"].as_str().unwrap();

    let batch = format!(
        r#"{{"{text_id}":"  admin@mmrs.invalid ","{sel_id}":"{opt}","{multi_id}":["{m3}","{m1}"]}}"#
    );
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::PATCH,
        VALUES,
        Some(&batch),
    )
    .await;
    assert_eq!(go.0, 200, "{}", String::from_utf8_lossy(&go.1));
    assert_eq!(rs.0, 200, "{}", String::from_utf8_lossy(&rs.1));
    assert_eq!(
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1),
        "the upsert response"
    );
    let response: serde_json::Value = serde_json::from_slice(&rs.1).unwrap();
    assert_eq!(
        response[text_id], "admin@mmrs.invalid",
        "the text value is trimmed"
    );
    assert_eq!(
        response[multi_id],
        serde_json::json!([m3, m1]),
        "order preserved"
    );

    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::GET,
        &user_values_path(me),
        None,
    )
    .await;
    assert_eq!(go.0, 200);
    assert_eq!(
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1),
        "the admin's own values"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&go.1).unwrap(),
        response
    );

    // Overwrite one, through the other server: the read through both sees it.
    let (s, b) = send(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        reqwest::Method::PATCH,
        VALUES,
        Some(&format!(r#"{{"{sel_id}":""}}"#)),
    )
    .await;
    assert_eq!(s, 200, "{}", String::from_utf8_lossy(&b));
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::GET,
        &user_values_path("me"),
        None,
    )
    .await;
    assert_eq!(
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&go.1).unwrap()[sel_id],
        ""
    );

    // A plain user's own, then the admin writing the plain user's, then the plain user reading
    // the admin's (readable — the read arm is "can see", the write arm "edit others").
    let plain_batch = format!(r#"{{"{text_id}":"plain@mmrs.invalid"}}"#);
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.plain.token,
        reqwest::Method::PATCH,
        VALUES,
        Some(&plain_batch),
    )
    .await;
    assert_eq!(
        (go.0, rs.0),
        (200, 200),
        "{} / {}",
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1)
    );
    assert_eq!(go.1, rs.1);
    let admin_writes_plain = format!(r#"{{"{sel_id}":"{opt}"}}"#);
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::PATCH,
        &user_values_path(&f.plain.id),
        Some(&admin_writes_plain),
    )
    .await;
    assert_eq!(
        (go.0, rs.0),
        (200, 200),
        "{} / {}",
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1)
    );
    assert_eq!(go.1, rs.1);
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.plain.token,
        reqwest::Method::GET,
        &user_values_path(me),
        None,
    )
    .await;
    assert_eq!((go.0, rs.0), (200, 200));
    assert_eq!(go.1, rs.1, "the plain user reads the admin's values");
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::GET,
        &user_values_path(&f.plain.id),
        None,
    )
    .await;
    assert_eq!(go.1, rs.1);
    let plain_values: serde_json::Value = serde_json::from_slice(&go.1).unwrap();
    assert_eq!(plain_values[text_id], "plain@mmrs.invalid");
    assert_eq!(plain_values[sel_id], opt);

    f.teardown().await;
}

/// The value refusals: validation per field type, the unknown field, the other-user permission,
/// the value permission on a managed field, the sync lock and the owner lock, and a field of the
/// wrong object type.
#[tokio::test]
async fn value_refusals_agree_on_the_licensed_pair() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let f = fixture("cpalverr").await;
    let me = logged_in_user_id();

    let text = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &text_field(&format!("{PREFIX}_etext")),
    )
    .await;
    let sel = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &select_field(&format!("{PREFIX}_esel")),
    )
    .await;
    let managed = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &format!(r#"{{"name":"{PREFIX}_eman","type":"text","attrs":{{"managed":"admin"}}}}"#),
    )
    .await;
    let user_field = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &format!(r#"{{"name":"{PREFIX}_euser","type":"user"}}"#),
    )
    .await;
    let text_id = text["id"].as_str().unwrap();
    let sel_id = sel["id"].as_str().unwrap();
    let managed_id = managed["id"].as_str().unwrap();
    let user_field_id = user_field["id"].as_str().unwrap();
    let synced_id = planted_id("sync1");
    plant_field(
        &synced_id,
        &format!("{PREFIX}_synced"),
        "text",
        r#"{"ldap":"department","visibility":"when_set"}"#,
    )
    .await;
    let owned_id = planted_id("owned1");
    plant_field(&owned_id, &format!("{PREFIX}_owned"), "text", r#"{"owners":[{"id":"com.example.hr","type":"plugin","scopes":[]}],"visibility":"when_set"}"#).await;
    let channel_id = planted_id("chan2");
    plant_field(&channel_id, &format!("{PREFIX}_chan2"), "text", "{}").await;
    let pool = common::fixture_pool().await.unwrap();
    sqlx::query("UPDATE propertyfields SET objecttype = 'channel' WHERE id = $1")
        .bind(&channel_id)
        .execute(&pool)
        .await
        .unwrap();
    let long = "x".repeat(65);

    let cases: Vec<(&str, &str, String, u16, &str)> = vec![
        (
            "not an option",
            &f.admin,
            format!(r#"{{"{sel_id}":"{NOWHERE}"}}"#),
            400,
            "app.property_value.validate.app_error",
        ),
        (
            "select given an array",
            &f.admin,
            format!(r#"{{"{sel_id}":["a"]}}"#),
            400,
            "app.property_value.validate.app_error",
        ),
        (
            "bad email",
            &f.admin,
            format!(r#"{{"{text_id}":"not an email"}}"#),
            400,
            "app.property_value.validate.app_error",
        ),
        (
            "text too long",
            &f.admin,
            format!(r#"{{"{text_id}":"{long}"}}"#),
            400,
            "app.property_value.validate.app_error",
        ),
        (
            "text given a number",
            &f.admin,
            format!(r#"{{"{text_id}":5}}"#),
            400,
            "app.property_value.validate.app_error",
        ),
        (
            "user field with a non-id",
            &f.admin,
            format!(r#"{{"{user_field_id}":"bob"}}"#),
            400,
            "app.property_value.validate.app_error",
        ),
        (
            "unknown field",
            &f.admin,
            format!(r#"{{"{NOWHERE}":"x"}}"#),
            404,
            "app.property_field.not_found.app_error",
        ),
        (
            "channel object type",
            &f.admin,
            format!(r#"{{"{channel_id}":"x"}}"#),
            404,
            "api.property_field.object_type_mismatch.app_error",
        ),
        (
            "managed field by a plain user",
            &f.plain.token,
            format!(r#"{{"{managed_id}":"x"}}"#),
            403,
            "api.property_value.patch.no_values_permission.app_error",
        ),
        (
            "sync-locked field",
            &f.admin,
            format!(r#"{{"{synced_id}":"x"}}"#),
            403,
            "app.property.sync_lock.app_error",
        ),
        (
            "owner-managed field",
            &f.admin,
            format!(r#"{{"{owned_id}":"x"}}"#),
            403,
            "app.property.access_denied.app_error",
        ),
        // The first refusal in batch order wins, and a valid value ahead of an invalid one is
        // not written.
        (
            "valid then invalid",
            &f.admin,
            format!(r#"{{"{sel_id}":"","{text_id}":"nope"}}"#),
            400,
            "app.property_value.validate.app_error",
        ),
    ];
    for (context, token, body, status, id) in &cases {
        let (go, rs) = both(
            &f.client,
            &f.pair,
            token,
            reqwest::Method::PATCH,
            VALUES,
            Some(body),
        )
        .await;
        same_error(&go, &rs, *status, id, context);
    }
    // The plain user writing the admin's values: the permission, before any field is read.
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.plain.token,
        reqwest::Method::PATCH,
        &user_values_path(me),
        Some(&format!(r#"{{"{NOWHERE}":"x"}}"#)),
    )
    .await;
    same_error(
        &go,
        &rs,
        403,
        "api.context.permissions.app_error",
        "plain user writing another's",
    );
    // The managed field **is** writable by an administrator.
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::PATCH,
        VALUES,
        Some(&format!(r#"{{"{managed_id}":"set by admin"}}"#)),
    )
    .await;
    assert_eq!(
        (go.0, rs.0),
        (200, 200),
        "{} / {}",
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1)
    );
    assert_eq!(go.1, rs.1);
    // Nothing from the refused batches landed.
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::GET,
        &user_values_path(me),
        None,
    )
    .await;
    assert_eq!(go.1, rs.1);
    let values: serde_json::Value = serde_json::from_slice(&go.1).unwrap();
    assert!(
        values.get(sel_id).is_none() && values.get(text_id).is_none(),
        "{values}"
    );

    f.teardown().await;
}

/// The read-side access modes, on planted fields the REST API cannot create: `source_only`
/// hides options and values from everyone but the source plugin; `shared_only` shows a caller
/// only the options and values it holds itself; a rank field shows everything at or below the
/// caller's own rank and clamps a higher target down to it.
#[tokio::test]
async fn access_modes_filter_reads_the_same_way_on_both() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let f = fixture("cpalmode").await;
    let me = logged_in_user_id();
    let plain = f.plain.id.as_str();

    let source_only = planted_id("srconly1");
    plant_field(&source_only, &format!("{PREFIX}_srconly"), "select",
        r#"{"protected":true,"source_plugin_id":"com.example.src","access_mode":"source_only","options":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"A","color":""}],"visibility":"always","sort_order":1}"#).await;
    let shared = planted_id("shared1");
    plant_field(&shared, &format!("{PREFIX}_shared"), "multiselect",
        r#"{"protected":true,"source_plugin_id":"com.example.src","access_mode":"shared_only","options":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"A","color":""},{"id":"bbbbbbbbbbbbbbbbbbbbbbbbbb","name":"B","color":""},{"id":"cccccccccccccccccccccccccc","name":"C","color":""}],"visibility":"always","sort_order":2}"#).await;
    let rank = planted_id("rank1");
    plant_field(&rank, &format!("{PREFIX}_rank"), "rank",
        r#"{"protected":true,"source_plugin_id":"com.example.src","access_mode":"shared_only","options":[{"id":"r1r1r1r1r1r1r1r1r1r1r1r1r1","name":"Low","color":"","rank":1},{"id":"r2r2r2r2r2r2r2r2r2r2r2r2r2","name":"Mid","color":"","rank":2},{"id":"r3r3r3r3r3r3r3r3r3r3r3r3r3","name":"High","color":"","rank":3}],"visibility":"always","sort_order":3}"#).await;
    let scalar = planted_id("scalar1");
    plant_field(&scalar, &format!("{PREFIX}_scalar"), "text",
        r#"{"protected":true,"source_plugin_id":"com.example.src","access_mode":"shared_only","visibility":"always","sort_order":4}"#).await;

    // Values: the admin holds A on source_only; [A,B] on shared; Mid on rank; "codename" on
    // scalar. The plain user holds A; [B,C]; High; "codename".
    plant_value(
        &planted_id("v1"),
        &source_only,
        me,
        r#""aaaaaaaaaaaaaaaaaaaaaaaaaa""#,
    )
    .await;
    plant_value(
        &planted_id("v2"),
        &shared,
        me,
        r#"["aaaaaaaaaaaaaaaaaaaaaaaaaa","bbbbbbbbbbbbbbbbbbbbbbbbbb"]"#,
    )
    .await;
    plant_value(
        &planted_id("v3"),
        &rank,
        me,
        r#""r2r2r2r2r2r2r2r2r2r2r2r2r2""#,
    )
    .await;
    plant_value(&planted_id("v4"), &scalar, me, r#""codename""#).await;
    plant_value(
        &planted_id("v5"),
        &source_only,
        plain,
        r#""aaaaaaaaaaaaaaaaaaaaaaaaaa""#,
    )
    .await;
    plant_value(
        &planted_id("v6"),
        &shared,
        plain,
        r#"["bbbbbbbbbbbbbbbbbbbbbbbbbb","cccccccccccccccccccccccccc"]"#,
    )
    .await;
    plant_value(
        &planted_id("v7"),
        &rank,
        plain,
        r#""r3r3r3r3r3r3r3r3r3r3r3r3r3""#,
    )
    .await;
    plant_value(&planted_id("v8"), &scalar, plain, r#""codename""#).await;

    // The field list as the admin: byte-identical, and shaped as the modes dictate.
    let list = list_agrees(&f.client, &f.pair, &f.admin, "the filtered field list").await;
    let by_id = |id: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(
        by_id(&source_only)["attrs"]["options"],
        serde_json::json!([]),
        "source_only: options hidden"
    );
    let shared_field = by_id(&shared);
    let shared_opts: Vec<&str> = shared_field["attrs"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        shared_opts,
        vec!["aaaaaaaaaaaaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbbbbbbbbbb"],
        "shared_only: only the options the caller holds"
    );
    let rank_field = by_id(&rank);
    let rank_opts: Vec<&str> = rank_field["attrs"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        rank_opts,
        vec!["Low", "Mid"],
        "rank: everything at or below the caller's own"
    );
    // The plain user's list too.
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.plain.token,
        reqwest::Method::GET,
        FIELDS,
        None,
    )
    .await;
    assert_eq!((go.0, rs.0), (200, 200));
    assert_eq!(go.1, rs.1, "the plain user's field list");

    // The plain user's values as the admin sees them.
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::GET,
        &user_values_path(plain),
        None,
    )
    .await;
    assert_eq!((go.0, rs.0), (200, 200));
    assert_eq!(
        String::from_utf8_lossy(&go.1),
        String::from_utf8_lossy(&rs.1),
        "the plain user's values, filtered for the admin"
    );
    let seen: serde_json::Value = serde_json::from_slice(&go.1).unwrap();
    assert!(seen.get(&source_only).is_none(), "source_only: dropped");
    assert_eq!(
        seen[&shared],
        serde_json::json!(["bbbbbbbbbbbbbbbbbbbbbbbbbb"]),
        "shared_only: the intersection"
    );
    assert_eq!(
        seen[&rank], "r2r2r2r2r2r2r2r2r2r2r2r2r2",
        "rank: High clamped down to the admin's Mid"
    );
    assert_eq!(seen[&scalar], "codename", "scalar: equal, so visible");

    // And the admin's values as the plain user sees them.
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.plain.token,
        reqwest::Method::GET,
        &user_values_path(me),
        None,
    )
    .await;
    assert_eq!((go.0, rs.0), (200, 200));
    assert_eq!(go.1, rs.1);
    let seen: serde_json::Value = serde_json::from_slice(&go.1).unwrap();
    assert_eq!(
        seen[&shared],
        serde_json::json!(["bbbbbbbbbbbbbbbbbbbbbbbbbb"])
    );
    assert_eq!(
        seen[&rank], "r2r2r2r2r2r2r2r2r2r2r2r2r2",
        "rank: Mid is at or below the plain user's High, so their own"
    );

    // The protected fields' **values** cannot be written by anyone over REST, and neither can
    // their definitions — but by the hook, not the handler: `protected` is an *attr* on these
    // rows, the column the handler's permission check reads is false, so the admin passes it and
    // meets `checkLegacyFieldWriteAccess` inside the service instead.
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::PATCH,
        VALUES,
        Some(&format!(r#"{{"{scalar}":"other"}}"#)),
    )
    .await;
    same_error(
        &go,
        &rs,
        403,
        "app.property.access_denied.app_error",
        "writing a protected field's value",
    );
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::PATCH,
        &field_path(&scalar),
        Some(r#"{"attrs":{"visibility":"hidden"}}"#),
    )
    .await;
    same_error(
        &go,
        &rs,
        403,
        "app.property.access_denied.app_error",
        "editing a protected field",
    );
    // And a protected field whose source plugin is **not installed** may be deleted by anyone
    // with the field permission — `checkFieldDeleteAccess` asks the plugin host, and neither
    // server has the plugin. Through us, then the row is gone through both.
    let (status, body) = send(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        reqwest::Method::DELETE,
        &field_path(&scalar),
        None,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let (go, rs) = both(
        &f.client,
        &f.pair,
        &f.admin,
        reqwest::Method::GET,
        FIELDS,
        None,
    )
    .await;
    assert_eq!(go.1, rs.1);
    assert!(
        !String::from_utf8_lossy(&go.1).contains(&scalar),
        "deleted on both"
    );

    f.teardown().await;
}

/// The four CPA websocket events and the generic property events beside them, one write per
/// server, compared scrubbed.
#[tokio::test]
async fn each_write_publishes_the_same_events_on_both() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = PROPERTY_ROWS.lock().await;
    let f = fixture("cpalevent").await;
    let me = logged_in_user_id();

    let mut go_probe = SocketProbe::connect(&f.pair.go, &f.admin).await;
    let mut rs_probe = SocketProbe::connect(&f.pair.rust, &f.admin).await;

    let go = create_via(
        &f.client,
        &f.pair.go,
        false,
        &f.admin,
        &text_field(&format!("{PREFIX}_ego")),
    )
    .await;
    let rs = create_via(
        &f.client,
        &f.pair.rust,
        true,
        &f.admin,
        &text_field(&format!("{PREFIX}_ers")),
    )
    .await;
    let go_id = go["id"].as_str().unwrap().to_owned();
    let rs_id = rs["id"].as_str().unwrap().to_owned();
    let patch = r#"{"attrs":{"visibility":"hidden"}}"#;
    assert_eq!(
        send(
            &f.client,
            &f.pair.go,
            false,
            &f.admin,
            reqwest::Method::PATCH,
            &field_path(&go_id),
            Some(patch)
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send(
            &f.client,
            &f.pair.rust,
            true,
            &f.admin,
            reqwest::Method::PATCH,
            &field_path(&rs_id),
            Some(patch)
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send(
            &f.client,
            &f.pair.go,
            false,
            &f.admin,
            reqwest::Method::PATCH,
            VALUES,
            Some(&format!(r#"{{"{go_id}":"a@b.io"}}"#))
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send(
            &f.client,
            &f.pair.rust,
            true,
            &f.admin,
            reqwest::Method::PATCH,
            VALUES,
            Some(&format!(r#"{{"{rs_id}":"a@b.io"}}"#))
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send(
            &f.client,
            &f.pair.go,
            false,
            &f.admin,
            reqwest::Method::DELETE,
            &field_path(&go_id),
            None
        )
        .await
        .0,
        200
    );
    assert_eq!(
        send(
            &f.client,
            &f.pair.rust,
            true,
            &f.admin,
            reqwest::Method::DELETE,
            &field_path(&rs_id),
            None
        )
        .await
        .0,
        200
    );

    for probe in [&mut go_probe, &mut rs_probe] {
        let found = probe
            .collect_until(Duration::from_secs(5), |frames| {
                frames
                    .iter()
                    .any(|fr| fr["event"] == "custom_profile_attributes_field_deleted")
            })
            .await;
        assert!(found, "the delete event never arrived");
    }

    let scrub_event = |mut ev: serde_json::Value| -> serde_json::Value {
        if let Some(field) = ev["data"].get_mut("field") {
            scrub(field, true);
        }
        if let Some(pf) = ev["data"].get_mut("property_field") {
            // A JSON **string** holding the field, on the generic events.
            let mut inner: serde_json::Value = serde_json::from_str(pf.as_str().unwrap()).unwrap();
            scrub(&mut inner, true);
            *pf = inner;
        }
        if let Some(values) = ev["data"].get_mut("values") {
            if let Some(s) = values.as_str() {
                let mut inner: serde_json::Value = serde_json::from_str(s).unwrap();
                if let Some(arr) = inner.as_array_mut() {
                    for v in arr {
                        scrub(v, false);
                        v.as_object_mut().unwrap().remove("field_id");
                    }
                }
                *values = inner;
            } else if let Some(obj) = values.as_object_mut() {
                let vals: Vec<serde_json::Value> = obj.values().cloned().collect();
                *values = serde_json::Value::Array(vals);
            }
        }
        ev["data"].as_object_mut().map(|d| d.remove("field_id"));
        ev.as_object_mut().map(|e| e.remove("seq"));
        ev
    };
    for event in [
        "custom_profile_attributes_field_created",
        "property_field_created",
        "custom_profile_attributes_field_updated",
        "property_field_updated",
        "custom_profile_attributes_values_updated",
        "property_values_updated",
        "custom_profile_attributes_field_deleted",
        "property_field_deleted",
    ] {
        let go_ev: Vec<serde_json::Value> = go_probe
            .events_named(event)
            .into_iter()
            .map(&scrub_event)
            .collect();
        let rs_ev: Vec<serde_json::Value> = rs_probe
            .events_named(event)
            .into_iter()
            .map(&scrub_event)
            .collect();
        assert!(!go_ev.is_empty(), "Go published no {event}");
        assert_eq!(go_ev, rs_ev, "{event}");
    }

    let _ = me;
    f.teardown().await;
}
