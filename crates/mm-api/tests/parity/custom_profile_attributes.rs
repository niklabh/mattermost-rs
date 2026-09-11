//! Cross-server parity for the seven custom-profile-attribute routes.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity custom_profile_attributes
//! ```
//!
//! # What is actually under test
//!
//! Not "does it refuse". The `access_control` property group is licence-gated by a hook whose
//! arms do **not** agree, so an unlicensed server gives five different answers across these seven
//! routes, and *which* one depends on the database:
//!
//! | | group holds no `user` field | group holds one |
//! |---|---|---|
//! | `GET /fields` | `200 []` | `403 app.property.license_error` |
//! | `GET /users/{id}/custom_profile_attributes` | `200 {}` | `200 {}` until a **value** exists |
//! | `PATCH`/`DELETE /fields/{id}` | `404 app.property.not_found.app_error` | `403` |
//! | `PATCH …/values` naming it | `404 app.property_field.not_found.app_error` | `403` |
//! | `POST /fields` | `403` | `403` |
//!
//! So half the tests here plant a row and half deliberately do not, and the planted ones are the
//! only evidence that the 200s are a short-circuit rather than a port that forgot to refuse.
//!
//! # Every test holds two locks
//!
//! [`ACTIVE_LICENCE_ROW`] read-side, because a licence row anywhere in the binary would make our
//! side forward and the comparison would be Go against Go. And [`CPA_ROWS`], because the planted
//! field is **global state for this whole family** — a test asserting `200 []` while a sibling
//! holds a field up is asserting nothing.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, assert_error_bodies_match_except_known_gaps, client,
    create_plain_user, create_team, delete_plain_user, fetch_both_raw, go_minted_token,
    logged_in_user_id, stack_enabled,
};

/// The `access_control` group's fields and values are one shared fixture for this file.
static CPA_ROWS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A valid 26-character id that names nothing.
const NOWHERE: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzz";

/// The planted field's id. 26 characters, `mmrs`-prefixed so a crashed run is identifiable.
const PLANTED_FIELD: &str = "mmrscpafieldparity00000001";
/// The planted value's id.
const PLANTED_VALUE: &str = "mmrscpavalueparity00000001";

const LICENCE_REFUSAL: &str = "app.property.license_error";

/// Plant one `user`-object field in the `access_control` group.
///
/// Go has no route that can create one without an Enterprise licence — that is the whole point of
/// the gate — so the row is written directly. Both servers read the same table, and the property
/// service caches **groups** only (`PropertyService.Group`), never fields, so Go sees it on the
/// next request with no cache flush.
async fn plant_field() -> bool {
    let Some(pool) = common::fixture_pool().await else {
        return false;
    };
    let Ok(group) = sqlx::query_scalar::<_, String>(
        "SELECT id FROM propertygroups WHERE name = 'access_control'",
    )
    .fetch_one(&pool)
    .await
    else {
        return false;
    };

    sqlx::query(
        "INSERT INTO propertyfields
            (id, groupid, name, type, attrs, targetid, targettype, objecttype, protected,
             createat, updateat, deleteat)
         VALUES ($1, $2, 'mmrsparity', 'text',
                 '{\"visibility\":\"when_set\",\"sort_order\":1,\"value_type\":\"\"}'::jsonb,
                 '', 'system', 'user', false, 1788600000000, 1788600000000, 0)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(PLANTED_FIELD)
    .bind(&group)
    .execute(&pool)
    .await
    .expect("the planted property field is written");

    true
}

/// Plant one value on the planted field for `user_id`.
async fn plant_value(user_id: &str) -> bool {
    let Some(pool) = common::fixture_pool().await else {
        return false;
    };
    let Ok(group) = sqlx::query_scalar::<_, String>(
        "SELECT id FROM propertygroups WHERE name = 'access_control'",
    )
    .fetch_one(&pool)
    .await
    else {
        return false;
    };

    sqlx::query(
        "INSERT INTO propertyvalues
            (id, targetid, targettype, groupid, fieldid, value, createat, updateat, deleteat)
         VALUES ($1, $2, 'user', $3, $4, '\"parity\"'::jsonb, 1788600000000, 1788600000000, 0)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(PLANTED_VALUE)
    .bind(user_id)
    .bind(&group)
    .bind(PLANTED_FIELD)
    .execute(&pool)
    .await
    .expect("the planted property value is written");

    true
}

/// Best-effort teardown. Called before the assertions so a failing test still leaves the group
/// empty for the next one.
async fn unplant() {
    let Some(pool) = common::fixture_pool().await else {
        return;
    };
    let _ = sqlx::query("DELETE FROM propertyvalues WHERE id = $1")
        .bind(PLANTED_VALUE)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM propertyfields WHERE id = $1")
        .bind(PLANTED_FIELD)
        .execute(&pool)
        .await;
}

/// One request to each server, as `fetch_both_raw` does for GETs, for any method and body.
async fn both_raw(
    client: &reqwest::Client,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<&[u8]>,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let send = async |base: &str| {
        let mut request = client
            .request(method.clone(), format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"));
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body.to_vec());
        }
        let response = request
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        if base == RUST {
            common::assert_served_by_rust(response.headers(), path);
        }
        (status, response.bytes().await.expect("body reads").to_vec())
    };

    (send(GO).await, send(RUST).await)
}

/// Both servers answered the same status, and the same error body up to the two documented gaps.
fn same_error(
    go: &(u16, Vec<u8>),
    rs: &(u16, Vec<u8>),
    want_status: u16,
    want_id: &str,
    context: &str,
) {
    assert_eq!(go.0, want_status, "{context}: Go's status");
    assert_eq!(rs.0, want_status, "{context}: our status");
    let body = assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, context);
    assert_eq!(
        body["id"].as_str(),
        Some(want_id),
        "{context}: the error id"
    );
}

/// The two reads that are **200s on an unlicensed server**, which is the half of this family a
/// flat refusal would get wrong.
#[tokio::test]
async fn the_empty_group_answers_two_hundred_twice() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = CPA_ROWS.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    unplant().await;

    let (go, rs) =
        fetch_both_raw(&client, &token, "/api/v4/custom_profile_attributes/fields").await;
    assert_eq!((go.0, rs.0), (200, 200), "the field list is a 200");
    assert_eq!(go.1, rs.1, "the field list body");
    // `[]` and the encoder's newline — not `null`, which is what a nil slice would have given.
    assert_eq!(String::from_utf8_lossy(&rs.1), "[]\n");

    let me = logged_in_user_id();
    let (go, rs) = fetch_both_raw(
        &client,
        &token,
        &format!("/api/v4/users/{me}/custom_profile_attributes"),
    )
    .await;
    assert_eq!((go.0, rs.0), (200, 200), "the value list is a 200");
    assert_eq!(go.1, rs.1, "the value list body");
    assert_eq!(String::from_utf8_lossy(&rs.1), "{}\n");
}

/// The same two reads with a row planted: the field list flips to 403, and the value list does
/// **not** — it needs a *value*, not a field. Two short-circuits, two different tables.
#[tokio::test]
async fn a_planted_row_flips_each_read_on_its_own_table() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = CPA_ROWS.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();
    unplant().await;
    if !plant_field().await {
        return;
    }

    let fields = fetch_both_raw(&client, &token, "/api/v4/custom_profile_attributes/fields").await;
    let values_before_value = fetch_both_raw(
        &client,
        &token,
        &format!("/api/v4/users/{me}/custom_profile_attributes"),
    )
    .await;
    plant_value(me).await;
    let values_after_value = fetch_both_raw(
        &client,
        &token,
        &format!("/api/v4/users/{me}/custom_profile_attributes"),
    )
    .await;
    unplant().await;

    same_error(
        &fields.0,
        &fields.1,
        403,
        LICENCE_REFUSAL,
        "a field makes the field list a refusal",
    );
    assert_eq!(
        (values_before_value.0.0, values_before_value.1.0),
        (200, 200),
        "a field alone does not touch the value list"
    );
    same_error(
        &values_after_value.0,
        &values_after_value.1,
        403,
        LICENCE_REFUSAL,
        "a value makes the value list a refusal",
    );
}

/// `POST /fields` is the one route with no empty-set escape — its hook is a *pre*-create — so it
/// refuses whatever the table holds. The body errors still come first.
#[tokio::test]
async fn creating_a_field_refuses_after_the_body_and_before_the_table() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = CPA_ROWS.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    unplant().await;

    const PATH: &str = "/api/v4/custom_profile_attributes/fields";
    for body in [
        &b""[..],
        &b"null"[..],
        &b"[]"[..],
        &b"5"[..],
        &br#""x""#[..],
        &b"{"[..],
    ] {
        let (go, rs) = both_raw(&client, &token, reqwest::Method::POST, PATH, Some(body)).await;
        same_error(
            &go,
            &rs,
            400,
            "api.context.invalid_body_param.app_error",
            &format!("POST /fields with {}", String::from_utf8_lossy(body)),
        );
    }

    let good = br#"{"name":"mmrsparity","type":"text","attrs":{"visibility":"when_set"}}"#;
    let (go, rs) = both_raw(&client, &token, reqwest::Method::POST, PATH, Some(good)).await;
    same_error(&go, &rs, 403, LICENCE_REFUSAL, "POST /fields, valid body");
}

/// A non-admin is refused by `PermissionManageSystem` and never learns whether the server is
/// licensed. The admin fixture cannot see this: `manage_system` answers yes to everything.
#[tokio::test]
async fn creating_a_field_needs_manage_system() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = CPA_ROWS.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let team = create_team(&client, &admin, "cpacreate").await;
    let plain = create_plain_user(&client, &admin, &team, "cpacreate").await;

    let (go, rs) = both_raw(
        &client,
        &plain.token,
        reqwest::Method::POST,
        "/api/v4/custom_profile_attributes/fields",
        Some(br#"{"name":"mmrsparity","type":"text"}"#),
    )
    .await;
    delete_plain_user(&client, &admin, &plain.id).await;

    same_error(
        &go,
        &rs,
        403,
        "api.context.permissions.app_error",
        "a plain user creating a CPA field",
    );
}

/// **The 404 is the finding.** `GetPropertyField` reads the row before the licence hook sees it,
/// so an unknown id is a not-found and only a real one is a refusal — and the two share a route.
#[tokio::test]
async fn the_field_writes_are_a_not_found_until_the_field_exists() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = CPA_ROWS.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    unplant().await;

    let missing = format!("/api/v4/custom_profile_attributes/fields/{NOWHERE}");
    let patch_missing = both_raw(
        &client,
        &token,
        reqwest::Method::PATCH,
        &missing,
        Some(br#"{"name":"x"}"#),
    )
    .await;
    let delete_missing = both_raw(&client, &token, reqwest::Method::DELETE, &missing, None).await;

    if !plant_field().await {
        return;
    }
    let planted = format!("/api/v4/custom_profile_attributes/fields/{PLANTED_FIELD}");
    let patch_planted = both_raw(
        &client,
        &token,
        reqwest::Method::PATCH,
        &planted,
        Some(br#"{"name":"x"}"#),
    )
    .await;
    let delete_planted = both_raw(&client, &token, reqwest::Method::DELETE, &planted, None).await;
    unplant().await;

    same_error(
        &patch_missing.0,
        &patch_missing.1,
        404,
        "app.property.not_found.app_error",
        "PATCH an unknown field",
    );
    same_error(
        &delete_missing.0,
        &delete_missing.1,
        404,
        "app.property.not_found.app_error",
        "DELETE an unknown field",
    );
    same_error(
        &patch_planted.0,
        &patch_planted.1,
        403,
        LICENCE_REFUSAL,
        "PATCH a real field",
    );
    same_error(
        &delete_planted.0,
        &delete_planted.1,
        403,
        LICENCE_REFUSAL,
        "DELETE a real field",
    );
}

/// Everything `patchCPAField` refuses before it reads anything, in the order it refuses it: the
/// url parameter, then the body, then the patch's own validation — **on the trimmed name**.
#[tokio::test]
async fn the_field_patch_refusals_come_in_order() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = CPA_ROWS.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    unplant().await;

    // A short id never reaches the body: `RequireFieldId` is the handler's first statement, so
    // this is an *url* param error even though the body is also invalid.
    let (go, rs) = both_raw(
        &client,
        &token,
        reqwest::Method::PATCH,
        "/api/v4/custom_profile_attributes/fields/abc",
        Some(b"["),
    )
    .await;
    same_error(
        &go,
        &rs,
        400,
        "api.context.invalid_url_param.app_error",
        "a short field id beats a broken body",
    );

    let path = format!("/api/v4/custom_profile_attributes/fields/{NOWHERE}");
    for body in [&b""[..], &b"null"[..], &b"[]"[..], &b"5"[..], &b"{"[..]] {
        let (go, rs) = both_raw(&client, &token, reqwest::Method::PATCH, &path, Some(body)).await;
        same_error(
            &go,
            &rs,
            400,
            "api.context.invalid_body_param.app_error",
            &format!("PATCH a field with {}", String::from_utf8_lossy(body)),
        );
    }

    // The trim runs *before* `IsValid`, so whitespace is an empty name and not a rename.
    for body in [
        &br#"{"name":""}"#[..],
        &br#"{"name":"   "}"#[..],
        &br#"{"type":"nope"}"#[..],
    ] {
        let (go, rs) = both_raw(&client, &token, reqwest::Method::PATCH, &path, Some(body)).await;
        same_error(
            &go,
            &rs,
            400,
            "model.property_field.is_valid.app_error",
            &format!("PATCH a field with {}", String::from_utf8_lossy(body)),
        );
    }

    // `{}` is a valid patch that changes nothing, so it gets past validation to the read — which
    // is the 404 above. That is what proves the three refusals are ordered and not a catch-all.
    let (go, rs) = both_raw(&client, &token, reqwest::Method::PATCH, &path, Some(b"{}")).await;
    same_error(
        &go,
        &rs,
        404,
        "app.property.not_found.app_error",
        "an empty patch is valid and reaches the read",
    );
}

/// `cpaPatchValues`' five refusals, in order, on both `/values` and `/users/{id}/…`.
#[tokio::test]
async fn the_value_patch_refusals_come_in_order() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = CPA_ROWS.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let me = logged_in_user_id();
    unplant().await;

    let bulk: String = {
        let entries: Vec<String> = (0..51)
            .map(|i| format!(r#""mmrscpabulk{i:015}":"x""#))
            .collect();
        format!("{{{}}}", entries.join(","))
    };

    let cases: Vec<(&[u8], u16, &str)> = vec![
        (b"", 400, "api.context.invalid_body_param.app_error"),
        (b"[]", 400, "api.context.invalid_body_param.app_error"),
        (b"5", 400, "api.context.invalid_body_param.app_error"),
        // `null` decodes into a nil map **without** an error, so it lands on the empty-batch id
        // rather than the body-param one. Four bytes, two different errors.
        (
            b"null",
            400,
            "api.property_value.patch.empty_body.app_error",
        ),
        (b"{}", 400, "api.property_value.patch.empty_body.app_error"),
        (
            br#"{"abc":"x"}"#,
            400,
            "api.property_value.patch.invalid_field_id.app_error",
        ),
        (
            br#"{"zzzzzzzzzzzzzzzzzzzzzzzzzz":"x"}"#,
            404,
            "app.property_field.not_found.app_error",
        ),
    ];

    for path in [
        "/api/v4/custom_profile_attributes/values".to_owned(),
        format!("/api/v4/users/{me}/custom_profile_attributes"),
    ] {
        for (body, status, id) in &cases {
            let (go, rs) =
                both_raw(&client, &token, reqwest::Method::PATCH, &path, Some(body)).await;
            same_error(
                &go,
                &rs,
                *status,
                id,
                &format!("PATCH {path} with {}", String::from_utf8_lossy(body)),
            );
        }

        // The cap is checked before any id is looked at, so fifty-one *unknown* ids report the
        // size and not the miss.
        let (go, rs) = both_raw(
            &client,
            &token,
            reqwest::Method::PATCH,
            &path,
            Some(bulk.as_bytes()),
        )
        .await;
        same_error(
            &go,
            &rs,
            400,
            "api.property_value.patch.too_many_items.request_error",
            &format!("PATCH {path} with fifty-one items"),
        );
    }
}

/// A batch naming a field that **does** exist gets past the cardinality check and hits the
/// licence. Without this, the 404 above is indistinguishable from a port that always 404s.
#[tokio::test]
async fn a_known_field_in_a_batch_reaches_the_licence() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = CPA_ROWS.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    unplant().await;
    if !plant_field().await {
        return;
    }

    let body = format!(r#"{{"{PLANTED_FIELD}":"x"}}"#);
    let known = both_raw(
        &client,
        &token,
        reqwest::Method::PATCH,
        "/api/v4/custom_profile_attributes/values",
        Some(body.as_bytes()),
    )
    .await;
    // One known id and one unknown one is still short a row, so it is the 404 again — the check
    // is `<`, not "none found".
    let mixed = format!(r#"{{"{PLANTED_FIELD}":"x","{NOWHERE}":"y"}}"#);
    let partial = both_raw(
        &client,
        &token,
        reqwest::Method::PATCH,
        "/api/v4/custom_profile_attributes/values",
        Some(mixed.as_bytes()),
    )
    .await;
    unplant().await;

    same_error(
        &known.0,
        &known.1,
        403,
        LICENCE_REFUSAL,
        "a batch of one known field",
    );
    same_error(
        &partial.0,
        &partial.1,
        404,
        "app.property_field.not_found.app_error",
        "a batch that is one row short",
    );
}

/// Writing another user's values needs `PermissionEditOtherUsers`; reading them does not.
///
/// The read arm is `UserCanSeeOtherUser`, which passes for every account on a stock server, so
/// the plain user gets the same `{}` the admin does — and the write arm refuses. Same route pair,
/// same actor, two answers.
#[tokio::test]
async fn another_users_values_are_readable_but_not_writable() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = CPA_ROWS.lock().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let me = logged_in_user_id();
    unplant().await;

    let team = create_team(&client, &admin, "cpaother").await;
    let plain = create_plain_user(&client, &admin, &team, "cpaother").await;
    let path = format!("/api/v4/users/{me}/custom_profile_attributes");

    let read = fetch_both_raw(&client, &plain.token, &path).await;
    let write = both_raw(
        &client,
        &plain.token,
        reqwest::Method::PATCH,
        &path,
        Some(br#"{"zzzzzzzzzzzzzzzzzzzzzzzzzz":"x"}"#),
    )
    .await;
    // Their own values still refuse on the *field*, not on the permission — so the 403 above is
    // the permission and not a blanket refusal of the route for a non-admin.
    let own_path = format!("/api/v4/users/{}/custom_profile_attributes", plain.id);
    let own = both_raw(
        &client,
        &plain.token,
        reqwest::Method::PATCH,
        &own_path,
        Some(br#"{"zzzzzzzzzzzzzzzzzzzzzzzzzz":"x"}"#),
    )
    .await;
    delete_plain_user(&client, &admin, &plain.id).await;

    assert_eq!(
        (read.0.0, read.1.0),
        (200, 200),
        "a plain user can read the admin's values"
    );
    assert_eq!(read.0.1, read.1.1, "and gets the same body");
    same_error(
        &write.0,
        &write.1,
        403,
        "api.context.permissions.app_error",
        "a plain user writing the admin's values",
    );
    same_error(
        &own.0,
        &own.1,
        404,
        "app.property_field.not_found.app_error",
        "a plain user writing their own values",
    );
}

/// `me` resolves to the session's user on both value routes, exactly as the literal id does.
#[tokio::test]
async fn me_is_the_session_user_on_both_value_routes() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let _rows = CPA_ROWS.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    unplant().await;

    let (go, rs) = fetch_both_raw(
        &client,
        &token,
        "/api/v4/users/me/custom_profile_attributes",
    )
    .await;
    assert_eq!((go.0, rs.0), (200, 200), "GET …/me/…");
    assert_eq!(go.1, rs.1, "GET …/me/… body");

    // A bad id is a url-param error before anything else, and `me` is substituted before the
    // check rather than after it.
    let (go, rs) = fetch_both_raw(
        &client,
        &token,
        "/api/v4/users/abc/custom_profile_attributes",
    )
    .await;
    same_error(
        &go,
        &rs,
        400,
        "api.context.invalid_url_param.app_error",
        "a short user id",
    );
}
