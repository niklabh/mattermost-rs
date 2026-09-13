//! Cross-server parity for `GET /api/v4/license/client` — `getClientLicense`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity license_client
//! ```
//!
//! # Two pairs of servers
//!
//! The stack's own pair is **Team Edition with no licence**, so `ClientLicense()` takes its
//! `nil` branch on both and the reachable surface there is the two 400s and the one 200. The
//! *licensed* map — the `read_license_information` branch and the sanitize list — is compared
//! against the licensed pair (`common::licensed`): the enterprise-ready Go oracle with the
//! stack's signed licence, and an mm-api carrying the same licence. Three callers there — an
//! administrator, a plain user, nobody — and two different maps between them.
//!
//! [`a_planted_id_without_a_verifying_row_is_not_a_licence`] pins the other boundary: a
//! `Systems.ActiveLicenseId` that names no `Licenses` row is what `LoadLicense` reads as "no
//! licence", on both servers, so it changes nothing on the wire.

use crate::common;

use common::{
    GO, RUST, a_team_and_channel_the_user_is_in, assert_error_bodies_match_except_known_gaps,
    client, create_plain_user, delete_plain_user, fetch_both_raw, fetch_licensed_pair,
    go_minted_token, licensed, stack_enabled,
};

const PATH: &str = "/api/v4/license/client";

/// Go's whole answer on an unlicensed server: 22 bytes, and **no trailing newline** —
/// `w.Write([]byte(model.MapToJSON(...)))` with no encoder (license.go:51).
const UNLICENSED: &[u8] = br#"{"IsLicensed":"false"}"#;

/// The lock now lives in [`common::ACTIVE_LICENCE_ROW`]: a second suite
/// (`recommended_channels`) proves the same boundary the same way, and two module-private locks
/// would not exclude each other.
use common::ACTIVE_LICENCE_ROW;

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

/// **The other boundary.** A `Systems.ActiveLicenseId` that names no `Licenses` row is not a
/// licence: `LoadLicense` looks the row up (platform/license.go:104) and finds nothing, and the
/// licence stays `nil`. Until 2026-09-13 this side read the id alone and forwarded on it; now it
/// reads the row, verifies it, and answers exactly what Go answers — the fallback map — itself.
/// Holds the shared lock exclusively, because the row is one for the whole installation.
#[tokio::test]
async fn a_planted_id_without_a_verifying_row_is_not_a_licence() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let path = format!("{PATH}?format=old");

    common::set_active_licence_id(None).await;
    // A 26-character id that passes `IsValidId` and matches no `Licenses` row. The length is
    // asserted by `set_active_licence_id`: a 25-character literal reads as valid, silently fails
    // `IsValidId`, and turns this test into one that proves the opposite of what it says.
    common::set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, &token, &path).await;
    // Malformed ids are *not* a licence either: `RemoveLicense` blanks the value rather than
    // deleting the row, so an empty string is the shape a de-licensed server actually leaves.
    common::set_active_licence_id(Some("")).await;
    let ((_, go_blanked), (_, rs_blanked)) = fetch_both_raw(&client, &token, &path).await;
    common::set_active_licence_id(None).await;

    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(
        go, UNLICENSED,
        "Go finds no row for the id and stays unlicensed"
    );
    assert_eq!(rs, go, "and so do we, served rather than forwarded");
    assert_eq!(go_blanked, UNLICENSED);
    assert_eq!(rs_blanked, go_blanked);
}

// ---------------------------------------------------------------------------------------------
// The licensed pair
// ---------------------------------------------------------------------------------------------

/// Nobody: the sanitized map, on both licensed servers, byte for byte. `APIHandler` needs no
/// session, and the zero-valued session Go carries for an anonymous caller has no roles, so
/// `read_license_information` is false and the seven keys are gone.
#[tokio::test]
async fn a_licensed_anonymous_caller_gets_the_sanitized_map() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let path = format!("{PATH}?format=old");
    let ((go_status, go), (rs_status, rs)) = fetch_licensed_pair(&client, &pair, None, &path).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "the sanitized client licence must be byte-identical"
    );
    let map: serde_json::Value = serde_json::from_slice(&go).unwrap();
    assert_eq!(map["IsLicensed"], "true", "the oracle is licensed");
    assert_eq!(map["SkuShortName"], "enterprise");
    for key in [
        "Id",
        "Name",
        "Email",
        "IssuedAt",
        "StartsAt",
        "ExpiresAt",
        "SkuName",
    ] {
        assert!(map.get(key).is_none(), "{key} is sanitized out");
    }
    assert!(!rs.ends_with(b"\n"), "no encoder, no newline");
}

/// The administrator: `read_license_information` is granted to `system_admin`, so the full map —
/// all forty keys, the customer's name and email and the three timestamps included.
#[tokio::test]
async fn a_licensed_administrator_gets_the_full_map() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let path = format!("{PATH}?format=old");
    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&client, &pair, Some(&token), &path).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "the full client licence must be byte-identical"
    );
    let map: serde_json::Value = serde_json::from_slice(&go).unwrap();
    assert_eq!(map["Id"], "mmrslicensedoracle00000001");
    assert_eq!(map["SkuName"], "Enterprise");
    assert_eq!(map["Email"], "oracle@mmrs.invalid");
    assert_eq!(
        map.as_object().unwrap().len(),
        40,
        "every key GetClientLicense writes"
    );
}

/// A plain user: a session, but not the permission — the sanitized map, same as nobody. This is
/// the case that separates "has a session" from "may read everything", which the anonymous test
/// alone cannot.
#[tokio::test]
async fn a_licensed_plain_user_gets_the_sanitized_map() {
    if !stack_enabled() {
        return;
    }
    let pair = licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "licmap").await;
    let path = format!("{PATH}?format=old");
    let ((go_status, go), (rs_status, rs)) =
        fetch_licensed_pair(&client, &pair, Some(&plain.token), &path).await;
    let ((_, anonymous), _) = fetch_licensed_pair(&client, &pair, None, &path).await;
    delete_plain_user(&client, &admin, &plain.id).await;

    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(String::from_utf8_lossy(&go), String::from_utf8_lossy(&rs));
    assert_eq!(
        go, anonymous,
        "a plain user and nobody get the same sanitized map"
    );
}
