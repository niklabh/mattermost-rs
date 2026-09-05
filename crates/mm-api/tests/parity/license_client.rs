//! Cross-server parity for `GET /api/v4/license/client` — `getClientLicense`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity license_client
//! ```
//!
//! # What this suite can and cannot reach
//!
//! The development stack is **Team Edition with no licence**, so `ClientLicense()` takes its
//! `nil` branch on both servers and the whole reachable surface is: the two 400s, the one 200,
//! and the forward. The *licensed* map — and with it the `read_license_information` branch and
//! the sanitize list — cannot be produced without a signed licence, and is not asserted here.
//! [`an_active_license_id_hands_the_route_back_to_go`] pins the boundary instead: the moment the
//! shared database says this installation is licensed, we stop answering.

use crate::common;

use common::{
    GO, RUST, assert_error_bodies_match_except_known_gaps, client, fetch_both_raw, go_minted_token,
    stack_enabled,
};

const PATH: &str = "/api/v4/license/client";

/// Go's whole answer on an unlicensed server: 22 bytes, and **no trailing newline** —
/// `w.Write([]byte(model.MapToJSON(...)))` with no encoder (license.go:51).
const UNLICENSED: &[u8] = br#"{"IsLicensed":"false"}"#;

/// **`Systems.ActiveLicenseId` is one row for the whole installation**, and this suite's last test
/// writes it. The harness runs these tests concurrently, so without a gate that write lands while
/// the others are mid-request and every one of them is forwarded — which they notice, because
/// `x-mmrs-served-by` is asserted, but only after five minutes of reading the wrong handler.
///
/// A read/write lock rather than a mutex: everything that expects the route to answer holds it
/// shared and still runs in parallel; the one test that makes the route *not* answer holds it
/// exclusively. Nothing outside this module reads or writes that row.
static ACTIVE_LICENCE_ROW: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// Fetch a path from both servers with **no credentials at all**.
///
/// The route is `api.APIHandler`, so this is a first-class case rather than an error one, and
/// [`common::fetch_both_raw`] cannot express it — it always sends a bearer token.
async fn fetch_both_anonymous(
    client: &reqwest::Client,
    path: &str,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let get = async |base: &str| {
        let response = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        if base == RUST {
            assert_eq!(
                response
                    .headers()
                    .get("x-mmrs-served-by")
                    .and_then(|v| v.to_str().ok()),
                Some("rust"),
                "{path} was forwarded, so this comparison proves nothing about the handler"
            );
        }
        (status, response.bytes().await.expect("body reads").to_vec())
    };

    (get(GO).await, get(RUST).await)
}

/// The one success, byte for byte, including the absent newline and the content type.
#[tokio::test]
async fn the_unlicensed_client_licence_is_byte_identical() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let path = format!("{PATH}?format=old");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;

    assert_eq!(go_status, 200, "an unlicensed server still answers 200");
    assert_eq!(rs_status, go_status);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "the client licence must be byte-identical"
    );
    assert_eq!(go, UNLICENSED, "and it is Go's `nil`-licence fallback map");
    assert!(
        !rs.ends_with(b"\n"),
        "license.go:51 writes the marshalled bytes; there is no encoder and no newline"
    );
    assert!(!go.ends_with(b"\n"), "and Go really does not write one");
}

/// `api.APIHandler` — no session. A logged-out browser asks this before it has anywhere to log in
/// *to*, so a 401 here would be a visible regression rather than a hardening.
#[tokio::test]
async fn no_session_is_required() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();

    let path = format!("{PATH}?format=old");
    let ((go_status, go), (rs_status, rs)) = fetch_both_anonymous(&client, &path).await;

    assert_eq!(
        go_status, 200,
        "an anonymous caller is answered, not refused"
    );
    assert_eq!(rs_status, go_status);
    assert_eq!(go, rs);
    assert_eq!(go, UNLICENSED);
}

/// The `Content-Type` a client parses on. `w.Write` inherits the handler's default header, so
/// this is `application/json` despite nothing in the handler setting it.
#[tokio::test]
async fn the_content_type_is_json_on_both() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let content_type = async |base: &str| {
        client
            .get(format!("{base}{PATH}?format=old"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("answers")
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    assert_eq!(content_type(GO).await.as_deref(), Some("application/json"));
    assert_eq!(
        content_type(RUST).await.as_deref(),
        Some("application/json")
    );
}

/// An absent **or empty** `format` is the route's own 400, not `SetInvalidParam`. Both spellings,
/// because `?format=` reaching the second branch is exactly the ordering mistake to make.
#[tokio::test]
async fn a_missing_or_empty_format_is_the_old_format_error() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for query in ["", "?", "?format=", "?other=old"] {
        let path = format!("{PATH}{query}");
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;

        assert_eq!(go_status, 400, "{path}: Go refuses a missing format");
        assert_eq!(rs_status, go_status, "{path}: statuses must match");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(
            body["id"], "api.license.client.old_format.app_error",
            "{path}: the route's own id, not the invalid-param one"
        );
    }
}

/// Anything else is `SetInvalidParam("format")` — a **different** id, and note it says *body*
/// param for a query-string parameter. The comparison is case-sensitive, so `OLD` lands here.
#[tokio::test]
async fn any_other_format_is_an_invalid_param() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    for query in ["?format=new", "?format=OLD", "?format=Old", "?format=old2"] {
        let path = format!("{PATH}{query}");
        let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;

        assert_eq!(go_status, 400, "{path}: Go refuses an unknown format");
        assert_eq!(rs_status, go_status, "{path}: statuses must match");
        let body = assert_error_bodies_match_except_known_gaps(&go, &rs, &path);
        assert_eq!(
            body["id"], "api.context.invalid_body_param.app_error",
            "{path}: `SetInvalidParam`, not the old-format error"
        );
        assert_eq!(
            body["detailed_error"], "",
            "{path}: `detailed_error` is wiped on both sides"
        );
    }
}

/// `url.Values.Get` returns the **first** value of a repeated key, so which one wins decides
/// between a 200 and a 400. A port that collected the last value would pass every test above.
#[tokio::test]
async fn a_repeated_format_takes_the_first_value() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let first_wins = format!("{PATH}?format=old&format=new");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &first_wins).await;
    assert_eq!(go_status, 200, "the first value is `old`, so this succeeds");
    assert_eq!(rs_status, go_status);
    assert_eq!(go, rs);

    let first_loses = format!("{PATH}?format=new&format=old");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &first_loses).await;
    assert_eq!(go_status, 400, "the first value is `new`, so this fails");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go, &rs, &first_loses);
}

/// Percent-escapes are decoded before the comparison, because `url.ParseQuery` decodes them.
#[tokio::test]
async fn a_percent_encoded_old_is_still_old() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let path = format!("{PATH}?format=%6Fld");
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    assert_eq!(go_status, 200, "`%6F` is `o`, so Go sees `old`");
    assert_eq!(rs_status, go_status);
    assert_eq!(go, rs);
}

/// **The boundary.** We answer only what we can see is unlicensed; the moment
/// `Systems.ActiveLicenseId` holds a valid id, the route goes back to the proxy.
///
/// Go is unmoved by the row — it loaded its licence at startup and re-reads only on a save — so
/// the *body* stays the unlicensed map and the observable difference is which server produced it.
/// That is the assertion: `x-mmrs-served-by`, not the bytes.
///
/// The row is cleared on the way **in** as well as out: an assertion panics past any teardown,
/// and a leftover row would silently forward this route for every later run, turning the suite
/// above into a comparison of Go against Go.
#[tokio::test]
async fn an_active_license_id_hands_the_route_back_to_go() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let Ok(url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .expect("the shared database is reachable");

    let clear = async || {
        sqlx::query("DELETE FROM systems WHERE name = 'ActiveLicenseId'")
            .execute(&pool)
            .await
            .expect("the active licence id is cleared");
    };
    clear().await;

    let client = client();
    let token = go_minted_token(&client).await;

    let served_by = async |token: &str| {
        client
            .get(format!("{RUST}{PATH}?format=old"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer")
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    assert_eq!(
        served_by(&token).await.as_deref(),
        Some("rust"),
        "with no licence row we answer the route ourselves"
    );

    // A 26-character id that passes `IsValidId` and matches no `Licenses` row — which is all
    // `LoadLicense` checks before it looks the licence up. The length is asserted rather than
    // counted by eye: a 25-character literal here reads as valid, silently fails `IsValidId`, and
    // turns this test into one that proves the opposite of what it says.
    const FAKE_LICENCE_ID: &str = "mmrslicence000000000000001";
    assert_eq!(
        FAKE_LICENCE_ID.len(),
        26,
        "`IsValidId` requires 26 characters"
    );
    sqlx::query("INSERT INTO systems (name, value) VALUES ('ActiveLicenseId', $1)")
        .bind(FAKE_LICENCE_ID)
        .execute(&pool)
        .await
        .expect("the active licence id is written");

    let forwarded = served_by(&token).await;

    // Malformed ids are *not* a licence: `RemoveLicense` blanks the value rather than deleting
    // the row, so an empty string is the shape a de-licensed server actually leaves behind.
    sqlx::query("UPDATE systems SET value = '' WHERE name = 'ActiveLicenseId'")
        .execute(&pool)
        .await
        .expect("the active licence id is blanked");
    let blanked = served_by(&token).await;

    clear().await;

    assert_eq!(
        forwarded.as_deref(),
        Some("go"),
        "a valid ActiveLicenseId means a licence we cannot render, so Go answers"
    );
    assert_eq!(
        blanked.as_deref(),
        Some("rust"),
        "a blanked id is what RemoveLicense leaves; that is not a licence"
    );
}
