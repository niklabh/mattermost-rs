//! Cross-server parity for the six export/import routes and the two upload-session reads.
//!
//! ```sh
//! docker compose up -d && scripts/go-server.sh start
//! scripts/parity.sh -p mm-api --test parity exports_and_uploads
//! ```
//!
//! # Two listings, two JSON encoders, one trailing byte
//!
//! `listExports` is `json.Marshal` + `w.Write`; `listImports` is
//! `json.NewEncoder(w).Encode`. So one body ends in `]` and the other in `]\n`, on two routes in
//! two files that do the same thing. Same for `getUpload` (newline) against `getUploadsForUser`
//! (none). Asserted as raw bytes, because a `serde_json::Value` comparison cannot see it.
//!
//! # Every route here is `manage_system`, and the plain user proves it
//!
//! A non-admin must get the same 403 from both servers on all six, and `deleteExport` /
//! `deleteImport` must not have deleted anything on the way to it.

use crate::common;

use common::{
    GO, RUST, client, create_plain_user, delete_plain_user, fetch_both_raw, go_minted_token,
    logged_in_user_id, purge_api_fixtures, stack_enabled,
};

/// Fetch a path from both servers with an explicit method, returning `(status, body, served_by)`.
async fn request_both(
    client: &reqwest::Client,
    token: &str,
    method: reqwest::Method,
    path: &str,
) -> (
    (u16, Vec<u8>, Option<String>),
    (u16, Vec<u8>, Option<String>),
) {
    let send = async |base: &str| {
        let response = client
            .request(method.clone(), format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        let served_by = response
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(std::borrow::ToOwned::to_owned);
        let body = response.bytes().await.expect("body reads").to_vec();
        (status, body, served_by)
    };
    (send(GO).await, send(RUST).await)
}

// ---------------------------------------------------------------------------------------------
// listings
// ---------------------------------------------------------------------------------------------

/// `GET /api/v4/exports` and `GET /api/v4/imports` — identical bodies, **different** trailing
/// bytes.
#[tokio::test]
async fn the_two_listings_agree_and_disagree_about_the_newline() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let (go, rust) = fetch_both_raw(&client, &token, "/api/v4/exports").await;
    assert_eq!(go.0, 200);
    assert_eq!(go.1, rust.1, "listExports body");
    assert!(
        !go.1.ends_with(b"\n"),
        "listExports is Marshal + Write, so no trailing newline: {:?}",
        String::from_utf8_lossy(&go.1)
    );
    assert!(
        serde_json::from_slice::<Vec<String>>(&go.1).is_ok(),
        "the body is always an array, never null: {:?}",
        String::from_utf8_lossy(&go.1)
    );

    let (go, rust) = fetch_both_raw(&client, &token, "/api/v4/imports").await;
    assert_eq!(go.0, 200);
    assert_eq!(go.1, rust.1, "listImports body");
    assert!(
        go.1.ends_with(b"\n"),
        "listImports is Encoder.Encode, so it carries one: {:?}",
        String::from_utf8_lossy(&go.1)
    );
    assert!(serde_json::from_slice::<Vec<String>>(&go.1).is_ok());
}

// ---------------------------------------------------------------------------------------------
// the archive routes
// ---------------------------------------------------------------------------------------------

/// `GET /api/v4/exports/{name}` for a name that is not there — a 404 with the export route's own
/// error id, which is not any of the file family's.
#[tokio::test]
async fn downloading_a_missing_export_matches() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let (go, rust) = fetch_both_raw(&client, &token, "/api/v4/exports/mmrs-not-here.zip").await;
    assert_eq!(go.0, 404, "no such export");
    assert_eq!(go.0, rust.0);
    common::assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, "downloadExport");
}

/// **Deleting a name that was never there succeeds.** Both the export and the import delete
/// answer `{"status":"OK"}` rather than a 404 — unlike `deleteBrandImage`, which 404s the same
/// situation.
#[tokio::test]
async fn deleting_a_missing_archive_is_a_success_on_both_routes() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for path in [
        "/api/v4/exports/mmrs-not-here.zip",
        "/api/v4/imports/mmrs-not-here.zip",
    ] {
        let (go, rust) = request_both(&client, &token, reqwest::Method::DELETE, path).await;
        assert_eq!(go.0, 200, "{path}: a missing archive is not a 404");
        assert_eq!(go.0, rust.0, "{path}: status");
        assert_eq!(go.1, rust.1, "{path}: body");
        assert_eq!(
            rust.2.as_deref(),
            Some("rust"),
            "{path} must be served here"
        );
    }
}

/// A name gorilla's `.+\.zip` would not have routed is **forwarded**, so Go answers its own mux
/// 404 rather than one of ours.
#[tokio::test]
async fn a_name_without_the_zip_suffix_is_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for path in [
        "/api/v4/exports/notazip",
        "/api/v4/exports/.zip",
        "/api/v4/imports/notazip",
    ] {
        let (go, rust) = request_both(&client, &token, reqwest::Method::GET, path).await;
        assert_eq!(go.0, rust.0, "{path}: status");
        assert_eq!(go.1, rust.1, "{path}: body");
        assert_eq!(
            rust.2.as_deref(),
            Some("go"),
            "{path} is outside the mux pattern and must be forwarded"
        );
    }
}

/// `POST /api/v4/exports/{name}/presign-url` — **always** refuses, at the feature flag, and the
/// error id carries Go's `eport` typo.
#[tokio::test]
async fn the_presign_url_route_always_refuses_with_gos_typo() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let (go, rust) = request_both(
        &client,
        &token,
        reqwest::Method::POST,
        "/api/v4/exports/mmrs-not-here.zip/presign-url",
    )
    .await;
    assert_eq!(go.0, 500);
    assert_eq!(go.0, rust.0, "status");
    assert_eq!(rust.2.as_deref(), Some("rust"));
    let body = common::assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, "presign-url");
    assert_eq!(
        body["id"].as_str(),
        Some("app.eport.generate_presigned_url.featureflag.app_error"),
        "the typo is Go's and is on the wire"
    );
}

// ---------------------------------------------------------------------------------------------
// permissions
// ---------------------------------------------------------------------------------------------

/// All six refuse a non-admin identically, and the two deletes refuse *before* touching a
/// directory.
#[tokio::test]
async fn a_plain_user_is_refused_on_every_route() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    purge_api_fixtures().await;
    let team_id = common::create_team(&client, &admin, "expteam").await;
    let plain = create_plain_user(&client, &admin, &team_id, "exp").await;

    let cases = [
        (reqwest::Method::GET, "/api/v4/exports"),
        (reqwest::Method::GET, "/api/v4/imports"),
        (reqwest::Method::GET, "/api/v4/exports/mmrs-not-here.zip"),
        (reqwest::Method::DELETE, "/api/v4/exports/mmrs-not-here.zip"),
        (reqwest::Method::DELETE, "/api/v4/imports/mmrs-not-here.zip"),
        (
            reqwest::Method::POST,
            "/api/v4/exports/mmrs-not-here.zip/presign-url",
        ),
    ];
    for (method, path) in cases {
        let (go, rust) = request_both(&client, &plain.token, method.clone(), path).await;
        assert_eq!(go.0, 403, "{method} {path}: a plain user is refused");
        assert_eq!(go.0, rust.0, "{method} {path}: status");
        common::assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, path);
        assert_eq!(
            rust.2.as_deref(),
            Some("rust"),
            "{method} {path} must be refused here, not forwarded"
        );
    }

    delete_plain_user(&client, &admin, &plain.id).await;
}

// ---------------------------------------------------------------------------------------------
// upload sessions
// ---------------------------------------------------------------------------------------------

/// `GET /api/v4/users/{user_id}/uploads` for the caller's own id — an empty array with **no**
/// trailing newline — and for someone else's, which is a 403 even for a system administrator.
#[tokio::test]
async fn the_upload_listing_is_self_only_even_for_an_admin() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let admin_id = logged_in_user_id().to_owned();
    purge_api_fixtures().await;
    let team_id = common::create_team(&client, &admin, "upteam").await;
    let plain = create_plain_user(&client, &admin, &team_id, "up").await;

    let (go, rust) = fetch_both_raw(
        &client,
        &admin,
        &format!("/api/v4/users/{admin_id}/uploads"),
    )
    .await;
    assert_eq!(go.0, 200);
    assert_eq!(go.1, rust.1, "own uploads body");
    assert!(
        !go.1.ends_with(b"\n"),
        "getUploadsForUser is Marshal + Write: {:?}",
        String::from_utf8_lossy(&go.1)
    );

    // Someone else's, as the **admin** — still a 403.
    let (go, rust) = fetch_both_raw(
        &client,
        &admin,
        &format!("/api/v4/users/{}/uploads", plain.id),
    )
    .await;
    assert_eq!(go.0, 403, "manage_system does not open this route");
    assert_eq!(go.0, rust.0);
    common::assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, "uploads for another user");

    // **`me` is resolved**, inside `RequireUserId` and before the id is validated — so this is
    // the caller's own list, byte for byte the same as the explicit-id request above. The first
    // version of this port validated the raw segment and answered 400 here; that is the divergence
    // this line exists to catch.
    let (go, rust) = fetch_both_raw(&client, &admin, "/api/v4/users/me/uploads").await;
    assert_eq!(go.0, 200, "`me` is the session user, not an invalid id");
    assert_eq!(go.0, rust.0, "`me` status");
    assert_eq!(go.1, rust.1, "`me` body");

    delete_plain_user(&client, &admin, &plain.id).await;
}

/// `GET /api/v4/uploads/{upload_id}` for an id that does not exist, and for one that is not an
/// id at all.
#[tokio::test]
async fn a_missing_upload_session_matches() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for id in ["mmrsupload0000000000000abc", "short"] {
        let path = format!("/api/v4/uploads/{id}");
        let (go, rust) = fetch_both_raw(&client, &token, &path).await;
        assert_eq!(go.0, rust.0, "{path}: status");
        common::assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, &path);
    }
}
