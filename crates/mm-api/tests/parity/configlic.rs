//! Cross-server parity for the configuration and licence **writes** — `config.go`'s three,
//! `license.go`'s four, and their six `*_local.go` twins on the socket.
//!
//! ```sh
//! scripts/parity.sh --test parity configlic
//! ```
//!
//! # What a write to a shared stack is allowed to do
//!
//! Both servers run on one `Configurations` row and the Go server holds its copy in memory, so a
//! configuration change here changes the Go server for every test that follows. The one test
//! that writes takes [`common::CONFIG_DOCUMENT`] exclusively, changes a key nothing else reads
//! (`EmailSettings.FeedbackName`), and restores the exact previous document whether or not its
//! assertions hold. Every other request in this file is a refusal — decided before anything is
//! saved — or a forward Go answers without writing.
//!
//! No licence is ever saved. The stack's own server refuses every upload at the signature; the
//! licensed oracle (`scripts/go-licensed.sh`) trusts a key pair on disk, and this suite signs a
//! **trial** licence with it so that `addLicense` reaches the trial gate — a 500 on a build with
//! no licence manager, before `SaveLicense` — and previews the oracle's own licence, which saves
//! nothing. `DELETE /license` against the licensed oracle would strip the licence from every
//! later test on the machine, so it is sent only as a user who is refused before it runs.

use crate::common;
use crate::common::local_socket::{go_socket, rust_socket, sockets_enabled};

use common::{GO, RUST, assert_served_by_rust, client, go_minted_token, stack_enabled};
use futures_util::FutureExt;

const CONFIG: &str = "/api/v4/config";
const CONFIG_PATCH: &str = "/api/v4/config/patch";
const CONFIG_RELOAD: &str = "/api/v4/config/reload";
const CONFIG_MIGRATE: &str = "/api/v4/config/migrate";
const LICENSE: &str = "/api/v4/license";
const LICENSE_PREVIEW: &str = "/api/v4/license/preview";
const TRIAL_LICENSE: &str = "/api/v4/trial-license";

const MULTIPART_BOUNDARY: &str = "mmrsconfiglicboundary";

/// One request to one base: `(status, headers, body)`, with an optional token, content type and
/// body — the writes here take JSON, multipart and nothing at all.
async fn send(
    base: &str,
    method: reqwest::Method,
    token: Option<&str>,
    path: &str,
    content_type: Option<&str>,
    body: Option<Vec<u8>>,
) -> (u16, reqwest::header::HeaderMap, Vec<u8>) {
    let mut request = client().request(method, format!("{base}{path}"));
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(content_type) = content_type {
        request = request.header("Content-Type", content_type);
    }
    if let Some(body) = body {
        request = request.body(body);
    }
    let response = request
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    (
        status,
        headers,
        response.bytes().await.expect("body reads").to_vec(),
    )
}

/// The same request to both servers, asserting ours answered it.
async fn both(
    method: reqwest::Method,
    token: Option<&str>,
    path: &str,
    content_type: Option<&str>,
    body: Option<Vec<u8>>,
) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
    let (go_status, _, go_body) =
        send(GO, method.clone(), token, path, content_type, body.clone()).await;
    let (rs_status, rs_headers, rs_body) =
        send(RUST, method, token, path, content_type, body).await;
    assert_served_by_rust(&rs_headers, path);
    ((go_status, go_body), (rs_status, rs_body))
}

fn served_by(headers: &reqwest::header::HeaderMap) -> Option<&str> {
    headers
        .get("x-mmrs-served-by")
        .and_then(|value| value.to_str().ok())
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("not JSON: {:?} ({e})", String::from_utf8_lossy(bytes)))
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// A `multipart/form-data` body with one file part.
fn multipart(part_name: &str, data: &[u8]) -> (String, Vec<u8>) {
    let mut body = format!(
        "--{MULTIPART_BOUNDARY}\r\nContent-Disposition: form-data; name=\"{part_name}\"; \
         filename=\"license.txt\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{MULTIPART_BOUNDARY}--\r\n").as_bytes());
    (
        format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}"),
        body,
    )
}

/// Both servers refuse identically: same status, same `id`, same `params`.
fn assert_refusal(
    go: &(u16, Vec<u8>),
    rust: &(u16, Vec<u8>),
    status: u16,
    id: &str,
    context: &str,
) -> serde_json::Value {
    assert_eq!(go.0, status, "{context}: Go's status ({})", text(&go.1));
    assert_eq!(rust.0, status, "{context}: our status ({})", text(&rust.1));
    let body = common::assert_error_bodies_match_except_known_gaps(&go.1, &rust.1, context);
    assert_eq!(body["id"], id, "{context}");
    body
}

/// One licence upload case: what it is, its content type, its body, and the refusal expected.
type Upload = (&'static str, Option<String>, Vec<u8>, u16, &'static str);

/// The oracle's signed licence, if `scripts/go-licensed.sh` has produced one on this machine.
fn oracle_signed_license() -> Option<String> {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/.build/license/license.signed"),
    )
    .ok()
}

/// A trial licence — `is_trial: true`, thirty days and eight hours long, so `IsTrialLicense`
/// and not `IsSanctionedTrial` — signed the way `scripts/go-licensed.sh` signs the oracle's:
/// SHA-512, PKCS#1 v1.5 with the private half of the pair the oracle trusts, the signature
/// appended, the whole thing base64.
fn sign_trial_license(pair: &common::LicensedPair) -> String {
    let private_key = std::path::Path::new(&pair.key_file).with_file_name("private.pem");
    assert!(
        private_key.exists(),
        "no private key beside {} — run scripts/go-licensed.sh start",
        pair.key_file
    );
    let starts_at: i64 = 1_767_225_600_000;
    let expires_at = starts_at + (30 * 24 + 8) * 60 * 60 * 1000;
    let plaintext = serde_json::json!({
        "id": "mmrsconfiglictrial00000001",
        "issued_at": starts_at,
        "starts_at": starts_at,
        "expires_at": expires_at,
        "customer": {"id": "mmrsconfigliccustomer00001", "name": "configlic trial", "email": "trial@mmrs.invalid", "company": "mattermost-rs"},
        "features": {"users": 100, "future_features": true},
        "sku_name": "Enterprise",
        "sku_short_name": "enterprise",
        "is_trial": true,
        "is_gov_sku": false,
        "is_non_production": false,
        "is_seat_count_enforced": false
    })
    .to_string();

    let dir = std::env::temp_dir().join(format!("mmrs-configlic-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let plaintext_path = dir.join("trial.json");
    std::fs::write(&plaintext_path, &plaintext).expect("the plaintext writes");
    let output = std::process::Command::new("openssl")
        .args(["dgst", "-sha512", "-sign"])
        .arg(&private_key)
        .arg(&plaintext_path)
        .output()
        .expect("openssl runs");
    assert!(
        output.status.success(),
        "openssl dgst failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout.len(),
        256,
        "a 2048-bit key signs in 256 bytes"
    );
    let _ = std::fs::remove_dir_all(&dir);

    let mut signed = plaintext.into_bytes();
    signed.extend_from_slice(&output.stdout);
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(signed)
}

/// `PluginSettings.EnableUploads` as the Go server runs it, so the patch tests can send the
/// *other* value and be refused.
async fn live_enable_uploads(admin: &str) -> bool {
    let (status, _, body) = send(GO, reqwest::Method::GET, Some(admin), CONFIG, None, None).await;
    assert_eq!(status, 200);
    json(&body)["PluginSettings"]["EnableUploads"]
        .as_bool()
        .expect("EnableUploads is a bool")
}

// ---------------------------------------------------------------------------------------------
// the configuration writes
// ---------------------------------------------------------------------------------------------

/// Every refusal `updateConfig` and `patchConfig` decide before the merge, as an administrator
/// and as a plain user — the decode, the permission, the cleared `SiteURL`, and the three
/// settings a patch may not move. None of these writes anything on either server.
#[tokio::test]
async fn the_config_write_refusals_match() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let enable_uploads = live_enable_uploads(&admin).await;
    let put = reqwest::Method::PUT;
    let json_ct = Some("application/json");

    // --- updateConfig: the decode, then the cleared SiteURL after SetDefaults.
    for (body, status, id, context) in [
        (
            "{}",
            400,
            "api.config.update_config.clear_siteurl.app_error",
            "PUT /config {} — SetDefaults leaves SiteURL empty while one is set",
        ),
        (
            r#"{"ServiceSettings":{"SiteURL":""}}"#,
            400,
            "api.config.update_config.clear_siteurl.app_error",
            "PUT /config with an explicit empty SiteURL",
        ),
        (
            r#"{"ServiceSettings":null}"#,
            400,
            "api.config.update_config.clear_siteurl.app_error",
            "PUT /config with a null section — a no-op decode, not a 400",
        ),
        (
            "null",
            400,
            "api.context.invalid_body_param.app_error",
            "PUT /config null",
        ),
        (
            "[]",
            400,
            "api.context.invalid_body_param.app_error",
            "PUT /config []",
        ),
        (
            "not json",
            400,
            "api.context.invalid_body_param.app_error",
            "PUT /config garbage",
        ),
        (
            "",
            400,
            "api.context.invalid_body_param.app_error",
            "PUT /config with an empty body",
        ),
        (
            r#"{"ServiceSettings":{"SiteURL":5}}"#,
            400,
            "api.context.invalid_body_param.app_error",
            "PUT /config with a wrongly typed setting",
        ),
    ] {
        let (go, rust) = both(
            put.clone(),
            Some(&admin),
            CONFIG,
            json_ct,
            Some(body.as_bytes().to_vec()),
        )
        .await;
        // `params` never reaches the wire — `AppError.ToJSON` carries the id, the translated
        // message, the detail, the request id and the status — so the parameter name lives in
        // Go's message alone and cannot be compared (D-092).
        assert_refusal(&go, &rust, status, id, context);
    }

    // --- patchConfig: the cleared SiteURL, then the three settings a patch may not move.
    let other_uploads = !enable_uploads;
    let uploads_patch = format!(r#"{{"PluginSettings":{{"EnableUploads":{other_uploads}}}}}"#);
    let marketplace_patch = format!(
        r#"{{"PluginSettings":{{"MarketplaceURL":"https://mmrs.invalid/marketplace","EnableUploads":{enable_uploads}}}}}"#
    );
    let mut patch_cases: Vec<(String, u16, &str, Option<&str>, &str)> = vec![
        (
            r#"{"ServiceSettings":{"SiteURL":""}}"#.to_owned(),
            400,
            "api.config.update_config.clear_siteurl.app_error",
            None,
            "PUT /config/patch clearing SiteURL",
        ),
        (
            uploads_patch,
            403,
            "api.config.update_config.not_allowed_security.app_error",
            Some("PluginSettings.EnableUploads"),
            "PUT /config/patch toggling EnableUploads",
        ),
        (
            r#"{"ImportSettings":{"Directory":"/mmrs-elsewhere"}}"#.to_owned(),
            403,
            "api.config.update_config.not_allowed_security.app_error",
            Some("ImportSettings.Directory"),
            "PUT /config/patch moving the import directory",
        ),
        (
            r#"{"ServiceSettings":null,"ImportSettings":{"Directory":"/mmrs-elsewhere"}}"#
                .to_owned(),
            403,
            "api.config.update_config.not_allowed_security.app_error",
            Some("ImportSettings.Directory"),
            "PUT /config/patch with a null section before the refused one",
        ),
        (
            "null".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
            None,
            "PUT /config/patch null",
        ),
        (
            "{".to_owned(),
            400,
            "api.context.invalid_body_param.app_error",
            None,
            "PUT /config/patch truncated",
        ),
    ];
    if enable_uploads {
        eprintln!(
            "PluginSettings.EnableUploads is on for this stack; the MarketplaceURL refusal \
             cannot fire and is not compared"
        );
    } else {
        patch_cases.push((
            marketplace_patch,
            403,
            "api.config.update_config.not_allowed_security.app_error",
            Some("PluginSettings.MarketplaceURL"),
            "PUT /config/patch moving the marketplace URL with uploads off",
        ));
    }
    for (body, status, id, name, context) in patch_cases {
        let (go, rust) = both(
            put.clone(),
            Some(&admin),
            CONFIG_PATCH,
            json_ct,
            Some(body.into_bytes()),
        )
        .await;
        // The refused setting's name is in Go's message only; `name` documents the case.
        let _ = name;
        assert_refusal(&go, &rust, status, id, context);
    }

    // --- a plain user: the permission sits after the decode on both writes, and is the first
    // thing `configReload` checks.
    let team = common::a_team_and_channel_the_user_is_in(&client, &admin)
        .await
        .0;
    let user = common::create_plain_user(&client, &admin, &team, "configlic").await;
    let plain = user.token.as_str();
    for (method, path, body, status, id, context) in [
        (
            put.clone(),
            CONFIG,
            "{}",
            403,
            "api.context.permissions.app_error",
            "PUT /config as a plain user",
        ),
        (
            put.clone(),
            CONFIG,
            "null",
            400,
            "api.context.invalid_body_param.app_error",
            "PUT /config null as a plain user — the decode comes first",
        ),
        (
            put.clone(),
            CONFIG_PATCH,
            "{}",
            403,
            "api.context.permissions.app_error",
            "PUT /config/patch as a plain user",
        ),
        (
            reqwest::Method::POST,
            CONFIG_RELOAD,
            "",
            403,
            "api.context.permissions.app_error",
            "POST /config/reload as a plain user",
        ),
    ] {
        let (go, rust) = both(
            method,
            Some(plain),
            path,
            json_ct,
            Some(body.as_bytes().to_vec()),
        )
        .await;
        assert_refusal(&go, &rust, status, id, context);
    }
    common::delete_plain_user(&client, &admin, &user.id).await;
}

/// The one write. A patch through this server is **forwarded** — Go merges, validates, persists
/// and swaps its in-memory copy — and the value is then visible from both servers' reads, ours
/// because `getConfig` re-reads the row per request. The same patch carries a `SiteURL`, which
/// is an environment override on this stack: it must move nothing, on either server or in the
/// row. Then the whole document goes back through `PUT /config`, and the reload, and the
/// original is restored — under the exclusive half of `CONFIG_DOCUMENT`, whatever happened.
#[tokio::test]
async fn a_forwarded_patch_is_seen_by_both_servers_and_cannot_move_an_environment_override() {
    if !stack_enabled() {
        return;
    }
    let _document = common::CONFIG_DOCUMENT.write().await;
    let admin = go_minted_token(&client()).await;

    let (status, _, original_bytes) =
        send(GO, reqwest::Method::GET, Some(&admin), CONFIG, None, None).await;
    assert_eq!(status, 200);
    let original = json(&original_bytes);
    let original_feedback = original["EmailSettings"]["FeedbackName"]
        .as_str()
        .expect("FeedbackName is a string")
        .to_owned();
    let live_site_url = original["ServiceSettings"]["SiteURL"]
        .as_str()
        .expect("SiteURL is a string")
        .to_owned();
    assert!(
        !live_site_url.is_empty(),
        "the stack runs with MM_SERVICESETTINGS_SITEURL; the override test needs one"
    );

    let restore = |admin: String, feedback: String| async move {
        let body = format!(
            r#"{{"EmailSettings":{{"FeedbackName":{}}}}}"#,
            serde_json::json!(feedback)
        );
        let (status, _, body) = send(
            GO,
            reqwest::Method::PUT,
            Some(&admin),
            CONFIG_PATCH,
            Some("application/json"),
            Some(body.into_bytes()),
        )
        .await;
        assert_eq!(status, 200, "restoring FeedbackName: {}", text(&body));
    };

    let nonce = format!("mmrs-parity-{}", mm_model::utils::new_id());
    let outcome = std::panic::AssertUnwindSafe(async {
        // The patch, through us: forwarded, and the answer is Go's new sanitized document.
        let patch = format!(
            r#"{{"EmailSettings":{{"FeedbackName":"{nonce}"}},"ServiceSettings":{{"SiteURL":"http://mmrs.invalid:1"}}}}"#
        );
        let (status, headers, body) = send(
            RUST,
            reqwest::Method::PUT,
            Some(&admin),
            CONFIG_PATCH,
            Some("application/json"),
            Some(patch.into_bytes()),
        )
        .await;
        assert_eq!(status, 200, "the forwarded patch: {}", text(&body));
        assert_eq!(
            served_by(&headers),
            Some("go"),
            "the save is Go's — see config_writes for why"
        );
        assert!(body.ends_with(b"\n"), "json.NewEncoder");
        let answered = json(&body);
        assert_eq!(answered["EmailSettings"]["FeedbackName"], nonce);
        assert_eq!(
            answered["ServiceSettings"]["SiteURL"], live_site_url,
            "an environment override is re-applied over the patch (config/store.go)"
        );

        // Both servers now read the new value; ours by re-reading the row.
        for base in [GO, RUST] {
            let (status, headers, body) =
                send(base, reqwest::Method::GET, Some(&admin), CONFIG, None, None).await;
            assert_eq!(status, 200);
            if base == RUST {
                assert_served_by_rust(&headers, CONFIG);
            }
            let read = json(&body);
            assert_eq!(read["EmailSettings"]["FeedbackName"], nonce, "{base} sees the patch");
            assert_eq!(read["ServiceSettings"]["SiteURL"], live_site_url, "{base}");
        }

        // The row: the new value persisted, the override **not** — `removeEnvOverrides` puts
        // the pre-override value back before the document is written.
        if let Some(pool) = common::fixture_pool().await {
            let row: (String,) = sqlx::query_as("SELECT value FROM configurations WHERE active")
                .fetch_one(&pool)
                .await
                .expect("one active row");
            let persisted: serde_json::Value = serde_json::from_str(&row.0).expect("JSON");
            assert_eq!(persisted["EmailSettings"]["FeedbackName"], nonce);
            assert_eq!(
                persisted["ServiceSettings"]["SiteURL"], "",
                "the persisted document never carries the environment's SiteURL"
            );
        }

        // The reload, through us: the permission is ours, the reload is Go's.
        let (status, headers, body) = send(
            RUST,
            reqwest::Method::POST,
            Some(&admin),
            CONFIG_RELOAD,
            None,
            None,
        )
        .await;
        assert_eq!(status, 200, "{}", text(&body));
        assert_eq!(served_by(&headers), Some("go"));
        assert_eq!(text(&body), r#"{"status":"OK"}"#);

        // The whole document through `PUT /config`, as the System Console sends it: the masked
        // secrets are desanitized by Go, the override is stripped again, and the answer is the
        // same document back.
        let mut full = json(&body_of_config(&admin).await);
        full["EmailSettings"]["FeedbackName"] = serde_json::Value::String(format!("{nonce}-put"));
        let (status, headers, body) = send(
            RUST,
            reqwest::Method::PUT,
            Some(&admin),
            CONFIG,
            Some("application/json"),
            Some(full.to_string().into_bytes()),
        )
        .await;
        assert_eq!(status, 200, "the forwarded PUT: {}", text(&body));
        assert_eq!(served_by(&headers), Some("go"));
        let answered = json(&body);
        assert_eq!(
            answered["EmailSettings"]["FeedbackName"],
            format!("{nonce}-put")
        );
        assert_eq!(answered["ServiceSettings"]["SiteURL"], live_site_url);
        assert_eq!(
            answered["SqlSettings"]["DataSource"], "********************************",
            "the answer is sanitized, and the row was desanitized (the server still runs)"
        );
    })
    .catch_unwind()
    .await;

    restore(admin.clone(), original_feedback).await;

    // The restore is itself the last assertion: Go's document is byte-identical to the one
    // this test started from.
    let (status, _, restored) =
        send(GO, reqwest::Method::GET, Some(&admin), CONFIG, None, None).await;
    assert_eq!(status, 200);
    assert_eq!(
        json(&restored),
        original,
        "the configuration document must be exactly what it was"
    );

    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

async fn body_of_config(admin: &str) -> Vec<u8> {
    let (status, _, body) = send(GO, reqwest::Method::GET, Some(admin), CONFIG, None, None).await;
    assert_eq!(status, 200);
    body
}

// ---------------------------------------------------------------------------------------------
// the licence writes, unlicensed
// ---------------------------------------------------------------------------------------------

/// On the stack's own server every licence upload ends at the signature — the key its validator
/// trusts is Mattermost's — so the four routes reduce to the permission, the multipart parse, the
/// signature, `RemoveLicense`'s no-op and `requestTrialLicense`'s nil manager. All served.
#[tokio::test]
async fn the_license_write_refusals_match_unlicensed() {
    if !stack_enabled() {
        return;
    }
    // `RemoveLicense` and our `App::license` both ask whether a licence is in force.
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let post = reqwest::Method::POST;

    let mut uploads: Vec<Upload> = vec![
        (
            "no body and no content type",
            None,
            Vec::new(),
            400,
            "api.license.parse_license.parse_form.app_error",
        ),
        (
            "a JSON body",
            Some("application/json".to_owned()),
            b"{}".to_vec(),
            400,
            "api.license.parse_license.parse_form.app_error",
        ),
        (
            "multipart with a boundary and no delimiter",
            Some(format!(
                "multipart/form-data; boundary={MULTIPART_BOUNDARY}"
            )),
            b"garbage".to_vec(),
            400,
            "api.license.parse_license.parse_form.app_error",
        ),
    ];
    let (content_type, body) = multipart("other", b"abc");
    uploads.push((
        "a form with no license part",
        Some(content_type),
        body,
        400,
        "api.license.add_license.no_file.app_error",
    ));
    let (content_type, body) = multipart("license", b"not a licence at all");
    uploads.push((
        "a licence that is not base64",
        Some(content_type),
        body,
        400,
        "api.license.add_license.invalid.app_error",
    ));
    let (content_type, body) = multipart("license", b"YWJj");
    uploads.push((
        "a licence too short to carry a signature",
        Some(content_type),
        body,
        400,
        "api.license.add_license.invalid.app_error",
    ));
    if let Some(signed) = oracle_signed_license() {
        let (content_type, body) = multipart("license", signed.trim().as_bytes());
        uploads.push((
            "the oracle's licence, which the production key does not verify",
            Some(content_type),
            body,
            400,
            "api.license.add_license.invalid.app_error",
        ));
    }

    for path in [LICENSE, LICENSE_PREVIEW] {
        for (what, content_type, body, status, id) in &uploads {
            let context = format!("POST {path} with {what}");
            let (go, rust) = both(
                post.clone(),
                Some(&admin),
                path,
                content_type.as_deref(),
                Some(body.clone()),
            )
            .await;
            assert_refusal(&go, &rust, *status, id, &context);
        }
    }

    // `RemoveLicense` with no licence in force: `ReturnStatusOK`, no newline, nothing written.
    let (go, rust) = both(reqwest::Method::DELETE, Some(&admin), LICENSE, None, None).await;
    assert_eq!(go.0, 200);
    assert_eq!(rust.0, 200);
    assert_eq!(text(&go.1), text(&rust.1));
    assert_eq!(text(&rust.1), r#"{"status":"OK"}"#);

    // `requestTrialLicense`: the permission, then the nil licence manager — a 403 with the
    // upgrade id, before the body is read.
    for body in ["{}", "not json", ""] {
        let (go, rust) = both(
            post.clone(),
            Some(&admin),
            TRIAL_LICENSE,
            Some("application/json"),
            Some(body.as_bytes().to_vec()),
        )
        .await;
        assert_refusal(
            &go,
            &rust,
            403,
            "api.license.upgrade_needed.app_error",
            &format!("POST /trial-license with body {body:?}"),
        );
    }

    // A plain user is refused first on all four, whatever the body.
    let team = common::a_team_and_channel_the_user_is_in(&client, &admin)
        .await
        .0;
    let user = common::create_plain_user(&client, &admin, &team, "configlicplain").await;
    for (method, path) in [
        (post.clone(), LICENSE),
        (post.clone(), LICENSE_PREVIEW),
        (reqwest::Method::DELETE, LICENSE),
        (post.clone(), TRIAL_LICENSE),
    ] {
        let context = format!("{method} {path} as a plain user");
        let (go, rust) = both(method, Some(&user.token), path, None, None).await;
        assert_refusal(
            &go,
            &rust,
            403,
            "api.context.permissions.app_error",
            &context,
        );
    }
    common::delete_plain_user(&client, &admin, &user.id).await;
}

// ---------------------------------------------------------------------------------------------
// the licence writes, licensed
// ---------------------------------------------------------------------------------------------

/// The branches past the signature, against the licensed pair, which trusts the key pair on
/// disk: `previewLicense` echoes the oracle's own licence — the parsed struct, encoded, never
/// saved — and `addLicense` with a freshly signed **trial** licence reaches the trial gate and
/// the nil licence manager behind it, before `SaveLicense`. Nothing here writes a licence.
#[tokio::test]
async fn the_licensed_pair_reaches_the_branches_past_the_signature() {
    if !stack_enabled() {
        return;
    }
    let pair = common::licensed().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let post = reqwest::Method::POST;

    let (content_type, body) = multipart("license", pair.signed.trim().as_bytes());
    let (go_status, _, go_body) = send(
        &pair.go,
        post.clone(),
        Some(&admin),
        LICENSE_PREVIEW,
        Some(&content_type),
        Some(body.clone()),
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &pair.rust,
        post.clone(),
        Some(&admin),
        LICENSE_PREVIEW,
        Some(&content_type),
        Some(body),
    )
    .await;
    assert_served_by_rust(&rs_headers, LICENSE_PREVIEW);
    assert_eq!(go_status, 200, "{}", text(&go_body));
    assert_eq!(rs_status, 200, "{}", text(&rs_body));
    assert_eq!(
        text(&go_body),
        text(&rs_body),
        "the previewed licence must match byte for byte"
    );
    let previewed = json(&rs_body);
    assert_eq!(previewed["id"], "mmrslicensedoracle00000001");
    assert_eq!(previewed["sku_short_name"], "enterprise");

    // The trial gate: valid signature, `IsTrialLicense`, not sanctioned — the licence manager
    // is asked and there is none. A 500, not the 403 `/trial-license` gives the same nil.
    let trial = sign_trial_license(&pair);
    let (content_type, body) = multipart("license", trial.as_bytes());
    let (go_status, _, go_body) = send(
        &pair.go,
        post.clone(),
        Some(&admin),
        LICENSE,
        Some(&content_type),
        Some(body.clone()),
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &pair.rust,
        post.clone(),
        Some(&admin),
        LICENSE,
        Some(&content_type),
        Some(body),
    )
    .await;
    assert_served_by_rust(&rs_headers, LICENSE);
    let refusal = assert_refusal(
        &(go_status, go_body),
        &(rs_status, rs_body),
        500,
        "api.license.upgrade_needed.app_error",
        "POST /license with a trial licence and no licence manager",
    );
    assert_eq!(refusal["status_code"], 500);

    // The same trial licence previews: the trial gate is `addLicense`'s alone.
    let (content_type, body) = multipart("license", trial.as_bytes());
    let (go_status, _, go_body) = send(
        &pair.go,
        post.clone(),
        Some(&admin),
        LICENSE_PREVIEW,
        Some(&content_type),
        Some(body.clone()),
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &pair.rust,
        post.clone(),
        Some(&admin),
        LICENSE_PREVIEW,
        Some(&content_type),
        Some(body),
    )
    .await;
    assert_served_by_rust(&rs_headers, LICENSE_PREVIEW);
    assert_eq!(go_status, 200, "{}", text(&go_body));
    assert_eq!(rs_status, 200);
    assert_eq!(text(&go_body), text(&rs_body));
    assert_eq!(json(&rs_body)["is_trial"], true);
    assert_eq!(
        json(&rs_body)["features"]["ldap"],
        serde_json::Value::Null,
        "previewLicense does not run Features.SetDefaults"
    );

    // The licence in force is still the oracle's, on both: nothing above saved anything.
    for base in [&pair.go, &pair.rust] {
        let (status, _, body) = send(
            base,
            reqwest::Method::GET,
            Some(&admin),
            "/api/v4/license/client?format=old",
            None,
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(json(&body)["Id"], "mmrslicensedoracle00000001", "{base}");
    }

    // `DELETE /license` on a licensed server would forward and strip the oracle's licence, so it
    // is sent only as a user the permission refuses first.
    let team = common::a_team_and_channel_the_user_is_in(&client, &admin)
        .await
        .0;
    let user = common::create_plain_user(&client, &admin, &team, "configliclic").await;
    let (go_status, _, go_body) = send(
        &pair.go,
        reqwest::Method::DELETE,
        Some(&user.token),
        LICENSE,
        None,
        None,
    )
    .await;
    let (rs_status, rs_headers, rs_body) = send(
        &pair.rust,
        reqwest::Method::DELETE,
        Some(&user.token),
        LICENSE,
        None,
        None,
    )
    .await;
    assert_served_by_rust(&rs_headers, LICENSE);
    assert_refusal(
        &(go_status, go_body),
        &(rs_status, rs_body),
        403,
        "api.context.permissions.app_error",
        "DELETE /license as a plain user on the licensed pair",
    );
    common::delete_plain_user(&client, &admin, &user.id).await;
}

// ---------------------------------------------------------------------------------------------
// the socket
// ---------------------------------------------------------------------------------------------

/// One request over one socket with an arbitrary content type.
async fn over_socket_with(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("Host", "localhost")
        .header("Content-Length", body.len().to_string());
    if let Some(content_type) = content_type {
        request = request.header("Content-Type", content_type);
    }
    let response = mm_api::local::send_over_unix(
        socket,
        request
            .body(axum::body::Body::from(body.to_vec()))
            .expect("builds"),
    )
    .await
    .unwrap_or_else(|e| panic!("{method} {path} over {}: {e}", socket.display()));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, headers, bytes)
}

/// The same request over both sockets: `(go, rust, served_here)`.
async fn both_sockets(
    method: &str,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> ((u16, Vec<u8>), (u16, axum::http::HeaderMap, Vec<u8>), bool) {
    let (go_status, _, go_body) = over_socket_with(
        &go_socket().expect("checked"),
        method,
        path,
        content_type,
        body,
    )
    .await;
    let (rs_status, rs_headers, rs_body) = over_socket_with(
        &rust_socket().expect("checked"),
        method,
        path,
        content_type,
        body,
    )
    .await;
    let served_here = rs_headers
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        (go_status, go_body),
        (rs_status, rs_headers, rs_body),
        served_here,
    )
}

/// The six socket pairs: `localUpdateConfig` and `localPatchConfig` to their decode,
/// `localMigrateConfig` to its two parameters, `configReload` forwarded, `localAddLicense` to
/// the signature with `http.Error`'s plain text where Go writes it, and `localRemoveLicense`
/// with no licence in force.
#[tokio::test]
async fn the_local_config_and_license_writes_match_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;
    let json_ct = Some("application/json");

    // --- the two config decodes and the migrate parameters, all served.
    for (method, path, body, id, name, context) in [
        (
            "PUT",
            CONFIG,
            "null",
            "api.context.invalid_body_param.app_error",
            "config",
            "PUT /config null",
        ),
        (
            "PUT",
            CONFIG,
            "[]",
            "api.context.invalid_body_param.app_error",
            "config",
            "PUT /config []",
        ),
        (
            "PUT",
            CONFIG,
            "",
            "api.context.invalid_body_param.app_error",
            "config",
            "PUT /config empty",
        ),
        (
            "PUT",
            CONFIG_PATCH,
            "not json",
            "api.context.invalid_body_param.app_error",
            "config",
            "PUT /config/patch garbage",
        ),
        (
            "PUT",
            CONFIG_PATCH,
            "null",
            "api.context.invalid_body_param.app_error",
            "config",
            "PUT /config/patch null",
        ),
        (
            "POST",
            CONFIG_MIGRATE,
            "{}",
            "api.context.invalid_body_param.app_error",
            "from",
            "migrate {}",
        ),
        (
            "POST",
            CONFIG_MIGRATE,
            "null",
            "api.context.invalid_body_param.app_error",
            "from",
            "migrate null",
        ),
        (
            "POST",
            CONFIG_MIGRATE,
            r#"{"to":"b"}"#,
            "api.context.invalid_body_param.app_error",
            "from",
            "migrate without from",
        ),
        (
            "POST",
            CONFIG_MIGRATE,
            r#"{"from":1,"to":"b"}"#,
            "api.context.invalid_body_param.app_error",
            "from",
            "migrate with a numeric from",
        ),
        (
            "POST",
            CONFIG_MIGRATE,
            r#"{"from":"a"}"#,
            "api.context.invalid_body_param.app_error",
            "to",
            "migrate without to",
        ),
        (
            "POST",
            CONFIG_MIGRATE,
            r#"{"from":"a","to":null}"#,
            "api.context.invalid_body_param.app_error",
            "to",
            "migrate with a null to",
        ),
    ] {
        let (go, (rs_status, _, rs_body), served) =
            both_sockets(method, path, json_ct, body.as_bytes()).await;
        assert!(served, "{context} must be served here");
        let _ = name; // in Go's message only, as above
        assert_refusal(&go, &(rs_status, rs_body), 400, id, context);
    }

    // A truncated migrate body is the one Go salvages a partial map from; forwarded.
    let (go, (rs_status, _, rs_body), served) =
        both_sockets("POST", CONFIG_MIGRATE, json_ct, br#"{"from":"a","to":"b""#).await;
    assert!(!served, "a body that does not decode is Go's to answer");
    assert_eq!(go.0, rs_status);
    common::local_socket::assert_forwarded_body_is_gos(&go.1, &rs_body, "migrate truncated");

    // `configReload` under APILocal: the permission passes by being unrestricted; the reload is
    // Go's and is forwarded.
    let (go, (rs_status, _, rs_body), served) =
        both_sockets("POST", CONFIG_RELOAD, None, b"").await;
    assert!(!served, "the reload is forwarded");
    assert_eq!(go.0, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(text(&go.1), text(&rs_body));
    assert_eq!(text(&rs_body), r#"{"status":"OK"}"#);

    // --- localAddLicense: `http.Error` plain text for the two pre-part failures, forwarded for
    // a failure inside the parse, `AppError`s past it.
    for (content_type, expected) in [
        (None, "request Content-Type isn't multipart/form-data\n"),
        (json_ct, "request Content-Type isn't multipart/form-data\n"),
        (
            Some("multipart/form-data"),
            "no multipart boundary param in Content-Type\n",
        ),
    ] {
        let (go, (rs_status, rs_headers, rs_body), served) =
            both_sockets("POST", LICENSE, content_type, b"{}").await;
        assert!(served, "{content_type:?}");
        assert_eq!(go.0, 400);
        assert_eq!(rs_status, 400);
        assert_eq!(text(&go.1), expected, "Go, {content_type:?}");
        assert_eq!(text(&rs_body), expected, "ours, {content_type:?}");
        assert_eq!(
            rs_headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("text/plain; charset=utf-8")
        );
    }
    let boundary_only = format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}");
    let (go, (rs_status, _, rs_body), served) =
        both_sockets("POST", LICENSE, Some(&boundary_only), b"garbage").await;
    assert!(
        !served,
        "a parse failure inside the body carries Go's own text"
    );
    assert_eq!(go.0, 400);
    assert_eq!(rs_status, 400);
    assert_eq!(text(&go.1), text(&rs_body));

    let (content_type, body) = multipart("other", b"abc");
    let (go, (rs_status, _, rs_body), served) =
        both_sockets("POST", LICENSE, Some(&content_type), &body).await;
    assert!(served);
    assert_refusal(
        &go,
        &(rs_status, rs_body),
        400,
        "api.license.add_license.no_file.app_error",
        "socket POST /license with no license part",
    );

    for (what, data) in [
        ("not base64", &b"not a licence"[..]),
        ("too short", &b"YWJj"[..]),
    ] {
        let (content_type, body) = multipart("license", data);
        let (go, (rs_status, _, rs_body), served) =
            both_sockets("POST", LICENSE, Some(&content_type), &body).await;
        assert!(served, "{what}");
        assert_refusal(
            &go,
            &(rs_status, rs_body),
            400,
            "api.license.add_license.invalid.app_error",
            &format!("socket POST /license with a licence that is {what}"),
        );
    }
    if let Some(signed) = oracle_signed_license() {
        let (content_type, body) = multipart("license", signed.trim().as_bytes());
        let (go, (rs_status, _, rs_body), served) =
            both_sockets("POST", LICENSE, Some(&content_type), &body).await;
        assert!(served);
        assert_refusal(
            &go,
            &(rs_status, rs_body),
            400,
            "api.license.add_license.invalid.app_error",
            "socket POST /license with the oracle's licence under the production key",
        );
    }

    // localRemoveLicense with no licence in force.
    let (go, (rs_status, _, rs_body), served) = both_sockets("DELETE", LICENSE, None, b"").await;
    assert!(served);
    assert_eq!(go.0, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(text(&go.1), text(&rs_body));
    assert_eq!(text(&rs_body), r#"{"status":"OK"}"#);
}
