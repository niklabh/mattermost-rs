//! Cross-server parity for the certificate and enterprise-gate routes of `api4/saml.go`,
//! `api4/ldap.go` and `api4/audit_logging.go` — the 22 HTTP pairs of `mm_api::auth_certs`.
//!
//! ```sh
//! scripts/go-licensed.sh start
//! scripts/parity.sh --test parity auth_certs
//! ```
//!
//! # What the rows are built to tell apart
//!
//! - **Check order.** A plain user posting to `/ldap/sync` unlicensed is the 501, not the 403
//!   (licence first); the same user posting `{}` to `/ldap/migrateid` is the 400 (body first)
//!   and `{"toAttribute":"x"}` the 403 (permission before licence). Each order is a row.
//! - **The decoder's shape.** `null`, `[]`, an empty body, a wrong type and a wrong-case key
//!   with a wrong type are each sent to the two body-decoding routes, because they are where
//!   `encoding/json` and serde disagree when left alone.
//! - **`MapFromJSON` keeps what fits.** `{"saml_metadata_url":"x","n":5}` carries the URL past
//!   the empty check in Go; a port that dropped the map on the first type error would answer
//!   the other 400.
//! - **A certificate is really added.** The public certificate and the private key are each
//!   posted through this server (forwarded) and `GET /saml/certificate/status` is compared
//!   *while one is present*, so the three `HasFile` reads have a `true` to get wrong.
//! - **The audit parser's extra refusal.** Two `certificate` parts are the `multiple_files`
//!   400 there and the first part everywhere else.
//!
//! # What it changes on the shared server, and what it puts back
//!
//! Each forwarded add writes a `ConfigurationFiles` row and a new `Configurations` revision, and
//! each remove deletes the row and sets the setting back to `""`. Removing a SAML certificate or
//! key also sets `SamlSettings.Encrypt` to `false` (app/saml.go:127), and **that one is restored**
//! at the end through `PUT /config/patch`: `licensed_sweep` compares the licensed Go's in-memory
//! configuration, which never reloads and still says the stock `true`, against the licensed
//! mm-api's read of the live row — measured, it failed on exactly this key until the restore.

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, GO, RUST, a_team_and_channel_the_user_is_in,
    assert_error_bodies_match_except_known_gaps, client, create_plain_user, delete_plain_user,
    fetch_both, go_minted_token, invalidate_go_caches, licensed, set_user_auth_service,
    stack_enabled,
};

const BOUNDARY: &str = "mmrsauthcertsparityboundary";

/// A self-signed certificate (`openssl req -x509`, CN `mmrs-authcerts-parity`, 100 years), as
/// the base64 body the `application/x-pem-file` arm of `addSamlIdpCertificate` expects: Go wraps
/// it in the `BEGIN`/`END` lines itself (app/saml.go:258).
const IDP_CERTIFICATE_BASE64: &str = "MIIDIzCCAgugAwIBAgIUXbwsGiFpv7tZjKlMQFw4fd1qfgwwDQYJKoZIhvcNAQEL\
BQAwIDEeMBwGA1UEAwwVbW1ycy1hdXRoY2VydHMtcGFyaXR5MCAXDTI2MDkxNTA3\
MjU0M1oYDzIxMjYwODIyMDcyNTQzWjAgMR4wHAYDVQQDDBVtbXJzLWF1dGhjZXJ0\
cy1wYXJpdHkwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQCrmI8L+CN5\
9cCZZAQ4j1Sw5nznW83esmXgtPB9jCNOK6kaKqDrlxt1EltdF3ukYrf+1kvtA+u6\
E+J3iODS/lFfMsLK/2m+nKvrb6SEeqUnVQpjs/1EogM45zYdM9gUprNdCCS/94KS\
Tie8F2Pc+8mWqEhoWvvL57OOgIlKi0D9APnZjGZCfPzCxYUuE5w3CmYZfdCyaiS2\
AcNqRzNpONbep18/5Cs9QqmoSc0fjrluUkCtDiXXQ+bw4wwq9qTFN+oT/OUo69sA\
qGfH3Dcuc3hFg/w6d1SqD2ZpEHLQzPopvAy+fHLoycQvqki1N16u/oz2IjWhPMyh\
2fxbv4O4AgW/AgMBAAGjUzBRMB0GA1UdDgQWBBTDkbJuBGtyvPgF3xHccmXqqP0S\
cjAfBgNVHSMEGDAWgBTDkbJuBGtyvPgF3xHccmXqqP0ScjAPBgNVHRMBAf8EBTAD\
AQH/MA0GCSqGSIb3DQEBCwUAA4IBAQAf0Qz8WYMYMhy1LFDp+DVKjFNLuJTY6hwX\
pF96H1xLETWtJwyxg2mg7N189aO8sYd+zjTF2NDhBq3uqEJ7ewHFJ4CFryRixEoU\
gQ4LfNylczWYveOA9j6gBgxXgfvw8+m/dj4Si7OEE9jtyRaClJlvtGeN/skyQITr\
5FQcRXTmZP9c/5dXwtPK3yRyaXG4RRIMVq6ElW4RFv5EZGCs//yAeXNC45Fp2IIF\
7HDENgkIRJ5feD6Jv8nGkGn4THVwTDMx3mPNWcJUPbB6XFvV7L/QoaCJhYojI1Fv\
Hitpe8CkSyRkJwgzqf70IQ5/055l6cGxj577Pu1Jg+jvYyEEYsh+";

/// A `multipart/form-data` body with one file part per `(name, content)`, and its content type.
fn multipart(parts: &[(&str, &str)]) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    for (name, content) in parts {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"cert.pem\"\r\nContent-Type: application/octet-stream\r\n\r\n{content}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

/// One request to one server: `(status, body, x-mmrs-served-by)`. The content type is the
/// caller's because half of these routes branch on it.
async fn send(
    client: &reqwest::Client,
    base: &str,
    method: reqwest::Method,
    token: Option<&str>,
    path: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> (u16, Vec<u8>, Option<String>) {
    let mut request = client.request(method, format!("{base}{path}"));
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    if let Some(content_type) = content_type {
        request = request.header("Content-Type", content_type);
    }
    let response = request
        .body(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
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

/// The same request to both servers of a pair, asserting **we** answered, and that the two
/// answers agree: byte for byte on a success, modulo the known error-body gaps otherwise.
/// Returns Go's status and body.
#[allow(clippy::too_many_arguments)]
async fn both(
    client: &reqwest::Client,
    pair: (&str, &str),
    method: reqwest::Method,
    token: Option<&str>,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
    expected: u16,
) -> Vec<u8> {
    let context = format!("{} {path} [{expected}]", method.as_str());
    let (go_status, go_body, _) = send(
        client,
        pair.0,
        method.clone(),
        token,
        path,
        content_type,
        body.to_vec(),
    )
    .await;
    let (rs_status, rs_body, served_by) = send(
        client,
        pair.1,
        method,
        token,
        path,
        content_type,
        body.to_vec(),
    )
    .await;
    assert_eq!(
        served_by.as_deref(),
        Some("rust"),
        "{context}: forwarded to Go, so this proves nothing"
    );
    assert_eq!(
        go_status,
        expected,
        "{context}: Go's status; body {}",
        String::from_utf8_lossy(&go_body)
    );
    assert_eq!(
        rs_status,
        expected,
        "{context}: our status; body {}",
        String::from_utf8_lossy(&rs_body)
    );
    if expected == 200 {
        assert_eq!(
            go_body,
            rs_body,
            "{context}: bodies differ\n go: {}\n rs: {}",
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body)
        );
    } else {
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
    }
    go_body
}

/// The same request to both servers where **ours is expected to forward**: asserts the forward,
/// equal statuses, and — since both bodies are then Go's — equal bodies apart from the
/// per-request id. Returns the shared status.
#[allow(clippy::too_many_arguments)]
async fn both_forwarded(
    client: &reqwest::Client,
    method: reqwest::Method,
    token: Option<&str>,
    path: &str,
    content_type: Option<&str>,
    body: &[u8],
) -> u16 {
    let context = format!("{} {path} (forwarded)", method.as_str());
    let (go_status, go_body, _) = send(
        client,
        GO,
        method.clone(),
        token,
        path,
        content_type,
        body.to_vec(),
    )
    .await;
    let (rs_status, rs_body, served_by) = send(
        client,
        RUST,
        method,
        token,
        path,
        content_type,
        body.to_vec(),
    )
    .await;
    assert_eq!(
        served_by.as_deref(),
        Some("go"),
        "{context}: expected a forward, got {served_by:?}"
    );
    assert_eq!(
        go_status,
        rs_status,
        "{context}: statuses differ\n go: {}\n rs: {}",
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body)
    );
    let strip = |body: &[u8]| -> serde_json::Value {
        let mut value: serde_json::Value =
            serde_json::from_slice(body).unwrap_or(serde_json::Value::Null);
        if let Some(object) = value.as_object_mut() {
            object.remove("request_id");
        }
        value
    };
    assert_eq!(strip(&go_body), strip(&rs_body), "{context}: bodies differ");
    go_status
}

/// `SamlSettings.Encrypt` as the stack's Go holds it.
async fn saml_encrypt(client: &reqwest::Client, admin: &str) -> bool {
    let config: serde_json::Value = client
        .get(format!("{GO}/api/v4/config"))
        .header("Authorization", format!("Bearer {admin}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the configuration is JSON");
    config["SamlSettings"]["Encrypt"]
        .as_bool()
        .expect("SamlSettings.Encrypt is a boolean")
}

/// Put `SamlSettings.Encrypt` back after the SAML removes flipped it — see the module doc.
async fn restore_saml_encrypt(client: &reqwest::Client, admin: &str, value: bool) {
    let response = client
        .put(format!("{GO}/api/v4/config/patch"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({ "SamlSettings": { "Encrypt": value } }))
        .send()
        .await
        .expect("Go answers");
    assert_eq!(
        response.status(),
        200,
        "the configuration patch is accepted"
    );
    assert_eq!(
        saml_encrypt(client, admin).await,
        value,
        "Encrypt is restored"
    );
}

fn json(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

const JSON: Option<&str> = Some("application/json");
const STACK: (&str, &str) = (GO, RUST);

// ---------------------------------------------------------------------------------------------
// api4/saml.go
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn saml_certificate_status_is_gated_and_read_from_the_live_document() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "authcertstatus").await;

    let path = "/api/v4/saml/certificate/status";
    let (go, rs) = fetch_both(&client, &admin, path).await;
    assert_eq!(go, rs, "the status bodies differ");
    assert!(
        go.ends_with(b"}\n"),
        "encoder-written: {}",
        String::from_utf8_lossy(&go)
    );

    both(
        &client,
        STACK,
        reqwest::Method::GET,
        Some(&plain.token),
        path,
        None,
        &[],
        403,
    )
    .await;
    both(
        &client,
        STACK,
        reqwest::Method::GET,
        None,
        path,
        None,
        &[],
        401,
    )
    .await;

    delete_plain_user(&client, &admin, &plain.id).await;
}

#[tokio::test]
async fn saml_metadata_from_idp_is_a_403_without_a_session_and_a_400_with_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "authcertmeta").await;
    let path = "/api/v4/saml/metadatafromidp";
    let post = reqwest::Method::POST;

    // `APIHandler`: no session is the permission check on the zero session — a 403, not a 401.
    both(
        &client,
        STACK,
        post.clone(),
        None,
        path,
        JSON,
        &json("{}"),
        403,
    )
    .await;
    both(
        &client,
        STACK,
        post.clone(),
        Some(&plain.token),
        path,
        JSON,
        &json(r#"{"saml_metadata_url":"idp.example"}"#),
        403,
    )
    .await;

    let invalid = both(
        &client,
        STACK,
        post.clone(),
        Some(&admin),
        path,
        JSON,
        &json("{}"),
        400,
    )
    .await;
    assert!(String::from_utf8_lossy(&invalid).contains("api.context.invalid_body_param.app_error"));
    let failure = both(
        &client,
        STACK,
        post.clone(),
        Some(&admin),
        path,
        JSON,
        &json(r#"{"saml_metadata_url":"idp.example"}"#),
        400,
    )
    .await;
    assert!(
        String::from_utf8_lossy(&failure)
            .contains("api.admin.saml.failure_get_metadata_from_idp.app_error")
    );
    // `MapFromJSON` keeps the string entries of an object whose other member does not fit.
    let partial = both(
        &client,
        STACK,
        post.clone(),
        Some(&admin),
        path,
        JSON,
        &json(r#"{"saml_metadata_url":"idp.example","n":5}"#),
        400,
    )
    .await;
    assert!(String::from_utf8_lossy(&partial).contains("failure_get_metadata_from_idp"));
    // …and nothing of a value that is not an object.
    let none = both(
        &client,
        STACK,
        post,
        Some(&admin),
        path,
        JSON,
        &json(r#"["idp.example"]"#),
        400,
    )
    .await;
    assert!(String::from_utf8_lossy(&none).contains("invalid_body_param"));

    delete_plain_user(&client, &admin, &plain.id).await;
}

#[tokio::test]
async fn reset_auth_data_decodes_the_body_and_then_meets_the_nil_interface() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "authcertreset").await;
    let path = "/api/v4/saml/reset_auth_data";
    let post = reqwest::Method::POST;

    both(
        &client,
        STACK,
        post.clone(),
        Some(&plain.token),
        path,
        JSON,
        &json("{}"),
        403,
    )
    .await;
    for body in [
        "{}",
        r#"{"include_deleted":null,"dry_run":true,"user_ids":[null,"x"]}"#,
        "{} trailing",
    ] {
        let out = both(
            &client,
            STACK,
            post.clone(),
            Some(&admin),
            path,
            JSON,
            &json(body),
            501,
        )
        .await;
        assert!(
            String::from_utf8_lossy(&out).contains("api.admin.saml.not_available.app_error"),
            "{body}"
        );
    }
    for body in [
        "null",
        "",
        "[]",
        r#"{"user_ids":"x"}"#,
        r#"{"include_deleted":"x"}"#,
        r#"{"DRY_RUN":"x"}"#,
        "{",
    ] {
        let out = both(
            &client,
            STACK,
            post.clone(),
            Some(&admin),
            path,
            JSON,
            &json(body),
            400,
        )
        .await;
        assert!(
            String::from_utf8_lossy(&out).contains("model.utils.decode_json.app_error"),
            "{body}"
        );
    }

    delete_plain_user(&client, &admin, &plain.id).await;
}

// ---------------------------------------------------------------------------------------------
// api4/ldap.go — the gates
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn ldap_routes_check_the_licence_first_except_migrateid_which_reads_the_body_first() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "authcertldap").await;
    let post = reqwest::Method::POST;

    for path in [
        "/api/v4/ldap/sync",
        "/api/v4/ldap/test",
        "/api/v4/ldap/test_connection",
        "/api/v4/ldap/test_diagnostics?test=filters",
    ] {
        for token in [&admin, &plain.token] {
            let out = both(
                &client,
                STACK,
                post.clone(),
                Some(token),
                path,
                JSON,
                &json("{}"),
                501,
            )
            .await;
            assert!(
                String::from_utf8_lossy(&out).contains("api.ldap_groups.license_error"),
                "{path}"
            );
        }
    }

    let path = "/api/v4/ldap/migrateid";
    for body in [
        "{}",
        r#"{"toAttribute":5}"#,
        r#"{"toAttribute":""}"#,
        "null",
        "",
    ] {
        for token in [&admin, &plain.token] {
            let out = both(
                &client,
                STACK,
                post.clone(),
                Some(token),
                path,
                JSON,
                &json(body),
                400,
            )
            .await;
            assert!(
                String::from_utf8_lossy(&out).contains("invalid_body_param"),
                "{body}"
            );
        }
    }
    both(
        &client,
        STACK,
        post.clone(),
        Some(&plain.token),
        path,
        JSON,
        &json(r#"{"toAttribute":"objectGUID"}"#),
        403,
    )
    .await;
    let out = both(
        &client,
        STACK,
        post,
        Some(&admin),
        path,
        JSON,
        &json(r#"{"toAttribute":"objectGUID"}"#),
        501,
    )
    .await;
    assert!(String::from_utf8_lossy(&out).contains("api.ldap_groups.license_error"));

    delete_plain_user(&client, &admin, &plain.id).await;
}

#[tokio::test]
async fn ldap_routes_on_the_licensed_pair_reach_the_nil_interface() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "authcertlic").await;
    let pair = licensed().await;
    let pair = (pair.go.as_str(), pair.rust.as_str());
    let post = reqwest::Method::POST;
    let disabled = |out: &[u8], context: &str| {
        assert!(
            String::from_utf8_lossy(out).contains("ent.ldap.disabled.app_error"),
            "{context}"
        );
    };

    // `syncLdap`: the job is never started, and the answer is the OK.
    let ok = both(
        &client,
        pair,
        post.clone(),
        Some(&admin),
        "/api/v4/ldap/sync",
        JSON,
        &[],
        200,
    )
    .await;
    assert_eq!(ok, br#"{"status":"OK"}"#);
    both(
        &client,
        pair,
        post.clone(),
        Some(&plain.token),
        "/api/v4/ldap/sync",
        JSON,
        &[],
        403,
    )
    .await;

    disabled(
        &both(
            &client,
            pair,
            post.clone(),
            Some(&admin),
            "/api/v4/ldap/test",
            JSON,
            &[],
            501,
        )
        .await,
        "test",
    );
    both(
        &client,
        pair,
        post.clone(),
        Some(&plain.token),
        "/api/v4/ldap/test",
        JSON,
        &[],
        403,
    )
    .await;

    let path = "/api/v4/ldap/test_connection";
    for body in [
        "{}",
        "null",
        r#"{"Enable":null,"LdapPort":389}"#,
        r#"{"LDAPPORT":389}"#,
    ] {
        disabled(
            &both(
                &client,
                pair,
                post.clone(),
                Some(&admin),
                path,
                JSON,
                &json(body),
                501,
            )
            .await,
            body,
        );
    }
    for body in [
        "[]",
        "",
        r#"{"Enable":"yes"}"#,
        r#"{"ENABLE":"yes"}"#,
        r#"{"LdapPort":1.5}"#,
        "5",
    ] {
        let out = both(
            &client,
            pair,
            post.clone(),
            Some(&admin),
            path,
            JSON,
            &json(body),
            400,
        )
        .await;
        assert!(
            String::from_utf8_lossy(&out).contains("invalid_body_param"),
            "{body}"
        );
    }
    both(
        &client,
        pair,
        post.clone(),
        Some(&plain.token),
        path,
        JSON,
        &json("{}"),
        403,
    )
    .await;

    let path = "/api/v4/ldap/test_diagnostics";
    for query in ["", "?test=", "?test=bogus", "?test=Filters"] {
        let out = both(
            &client,
            pair,
            post.clone(),
            Some(&admin),
            &format!("{path}{query}"),
            JSON,
            &json("{}"),
            400,
        )
        .await;
        assert!(
            String::from_utf8_lossy(&out).contains("invalid_body_param"),
            "{query}"
        );
    }
    for test in ["filters", "attributes", "group_attributes"] {
        disabled(
            &both(
                &client,
                pair,
                post.clone(),
                Some(&admin),
                &format!("{path}?test={test}"),
                JSON,
                &json("{}"),
                501,
            )
            .await,
            test,
        );
    }
    both(
        &client,
        pair,
        post.clone(),
        Some(&admin),
        &format!("{path}?test=filters"),
        JSON,
        &json("[]"),
        400,
    )
    .await;
    both(
        &client,
        pair,
        post.clone(),
        Some(&plain.token),
        &format!("{path}?test=filters"),
        JSON,
        &json("{}"),
        403,
    )
    .await;

    disabled(
        &both(
            &client,
            pair,
            post,
            Some(&admin),
            "/api/v4/ldap/migrateid",
            JSON,
            &json(r#"{"toAttribute":"objectGUID"}"#),
            501,
        )
        .await,
        "migrateid",
    );

    delete_plain_user(&client, &admin, &plain.id).await;
}

// ---------------------------------------------------------------------------------------------
// the certificate writes
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn certificate_adds_serve_the_gate_and_the_parse_and_forward_the_write() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "authcertadd").await;
    let post = reqwest::Method::POST;
    let delete = reqwest::Method::DELETE;
    let status_path = "/api/v4/saml/certificate/status";
    let saml_encrypt_before = saml_encrypt(&client, &admin).await;

    let (form_type, no_part) = multipart(&[("other", "x")]);
    let (_, one_part) = multipart(&[(
        "certificate",
        "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
    )]);
    let (_, two_parts) = multipart(&[("certificate", "a"), ("certificate", "b")]);

    // (path, the error id for an `application/json` body, whether two parts are refused). The
    // idp route branches on the content type before it parses, so its id is the type's.
    let families = [
        (
            "/api/v4/saml/certificate/public",
            "api.admin.add_certificate.no_file.app_error",
            false,
        ),
        (
            "/api/v4/saml/certificate/private",
            "api.admin.add_certificate.no_file.app_error",
            false,
        ),
        (
            "/api/v4/saml/certificate/idp",
            "api.admin.saml.set_certificate_from_metadata.invalid_content_type.app_error",
            false,
        ),
        (
            "/api/v4/ldap/certificate/public",
            "api.admin.add_certificate.parseform.app_error",
            false,
        ),
        (
            "/api/v4/ldap/certificate/private",
            "api.admin.add_certificate.parseform.app_error",
            false,
        ),
        (
            "/api/v4/audit_logs/certificate",
            "api.admin.add_certificate.no_file.app_error",
            true,
        ),
    ];
    for (path, parse_id, refuses_two) in families {
        both(
            &client,
            STACK,
            post.clone(),
            Some(&plain.token),
            path,
            JSON,
            &json("{}"),
            403,
        )
        .await;
        both(
            &client,
            STACK,
            delete.clone(),
            Some(&plain.token),
            path,
            None,
            &[],
            403,
        )
        .await;
        let out = both(
            &client,
            STACK,
            post.clone(),
            Some(&admin),
            path,
            JSON,
            &json("{}"),
            400,
        )
        .await;
        assert!(String::from_utf8_lossy(&out).contains(parse_id), "{path}");
        let out = both(
            &client,
            STACK,
            post.clone(),
            Some(&admin),
            path,
            Some(&form_type),
            &no_part,
            400,
        )
        .await;
        assert!(
            String::from_utf8_lossy(&out).contains("api.admin.add_certificate.no_file.app_error"),
            "{path}"
        );
        if refuses_two {
            let out = both(
                &client,
                STACK,
                post.clone(),
                Some(&admin),
                path,
                Some(&form_type),
                &two_parts,
                400,
            )
            .await;
            assert!(
                String::from_utf8_lossy(&out)
                    .contains("api.admin.add_certificate.multiple_files.app_error"),
                "{path}"
            );
        } else {
            // The first of two parts is taken, and the write is Go's.
            assert_eq!(
                both_forwarded(
                    &client,
                    post.clone(),
                    Some(&admin),
                    path,
                    Some(&form_type),
                    &two_parts
                )
                .await,
                200,
                "{path}"
            );
        }
        // A well-formed add is forwarded and Go writes it; the remove is forwarded too.
        assert_eq!(
            both_forwarded(
                &client,
                post.clone(),
                Some(&admin),
                path,
                Some(&form_type),
                &one_part
            )
            .await,
            200,
            "{path}"
        );
        if path.starts_with("/api/v4/saml/certificate/") {
            let (go, rs) = fetch_both(&client, &admin, status_path).await;
            assert_eq!(go, rs, "{path}: status while the file is present");
            let expected_key = match path.rsplit('/').next() {
                Some("public") => "\"public_certificate_file\":true",
                Some("private") => "\"private_key_file\":true",
                _ => "\"idp_certificate_file\":true",
            };
            assert!(
                String::from_utf8_lossy(&go).contains(expected_key),
                "{path}: Go reports the file: {}",
                String::from_utf8_lossy(&go)
            );
        }
        assert_eq!(
            both_forwarded(&client, delete.clone(), Some(&admin), path, None, &[]).await,
            200,
            "{path}"
        );
    }
    let (go, rs) = fetch_both(&client, &admin, status_path).await;
    assert_eq!(go, rs);
    assert_eq!(go, b"{\"idp_certificate_file\":false,\"private_key_file\":false,\"public_certificate_file\":false}\n");
    restore_saml_encrypt(&client, &admin, saml_encrypt_before).await;

    // `addSamlIdpCertificate` branches on `Content-Type` before it parses anything.
    let idp = "/api/v4/saml/certificate/idp";
    let out = both(
        &client,
        STACK,
        post.clone(),
        Some(&admin),
        idp,
        None,
        &json("x"),
        400,
    )
    .await;
    assert!(String::from_utf8_lossy(&out).contains("missing_content_type"));
    for content_type in [
        "text/plain",
        ";;bad",
        "multipart/form-data; boundary",
        "application/x-pem",
    ] {
        let out = both(
            &client,
            STACK,
            post.clone(),
            Some(&admin),
            idp,
            Some(content_type),
            &json("x"),
            400,
        )
        .await;
        assert!(
            String::from_utf8_lossy(&out).contains("invalid_content_type"),
            "{content_type}"
        );
    }
    // `multipart/form-data` with a boundary but no boundary in the body: the parse fails.
    let out = both(
        &client,
        STACK,
        post.clone(),
        Some(&admin),
        idp,
        Some(&form_type),
        &json("x"),
        400,
    )
    .await;
    assert!(String::from_utf8_lossy(&out).contains("api.admin.add_certificate.no_file.app_error"));
    // The PEM arm is a write and is Go's whole; the media type is matched case-folded. A body
    // Go cannot decode makes `SetSamlIdpCertificateFromMetadata` dereference a nil PEM block,
    // and net/http answers the panic by dropping the connection — nothing to compare — so this
    // row sends a certificate Go can parse and reads the status while it is installed.
    assert_eq!(
        both_forwarded(
            &client,
            post.clone(),
            Some(&admin),
            idp,
            Some("Application/X-PEM-File"),
            IDP_CERTIFICATE_BASE64.as_bytes(),
        )
        .await,
        200
    );
    let (go, rs) = fetch_both(&client, &admin, status_path).await;
    assert_eq!(
        go, rs,
        "status with the idp certificate installed through the PEM arm"
    );
    assert!(String::from_utf8_lossy(&go).contains("\"idp_certificate_file\":true"));
    assert_eq!(
        both_forwarded(&client, delete.clone(), Some(&admin), idp, None, &[]).await,
        200
    );
    let (go, rs) = fetch_both(&client, &admin, status_path).await;
    assert_eq!(go, rs);
    assert!(String::from_utf8_lossy(&go).contains("\"idp_certificate_file\":false"));

    delete_plain_user(&client, &admin, &plain.id).await;
}

#[tokio::test]
async fn group_sync_memberships_checks_the_permission_the_user_then_the_auth_service() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team_id, _) = a_team_and_channel_the_user_is_in(&client, &admin).await;
    let plain = create_plain_user(&client, &admin, &team_id, "authcertsync").await;
    let post = reqwest::Method::POST;
    let path = |id: &str| format!("/api/v4/ldap/users/{id}/group_sync_memberships");

    both(
        &client,
        STACK,
        post.clone(),
        Some(&plain.token),
        &path(&plain.id),
        None,
        &[],
        403,
    )
    .await;
    // No `RequireUserId`: `me` and any alphanumeric segment reach `GetUser` as sent.
    for id in ["me", "zzz", "abcdefghijklmnopqrstuvwxyz"] {
        let out = both(
            &client,
            STACK,
            post.clone(),
            Some(&admin),
            &path(id),
            None,
            &[],
            404,
        )
        .await;
        assert!(
            String::from_utf8_lossy(&out).contains("app.user.missing_account.const"),
            "{id}"
        );
    }
    // Outside the mux charset the route never matches, on either server.
    assert_eq!(
        both_forwarded(&client, post.clone(), Some(&admin), &path("a-b"), None, &[]).await,
        404
    );

    let out = both(
        &client,
        STACK,
        post.clone(),
        Some(&admin),
        &path(&plain.id),
        None,
        &[],
        400,
    )
    .await;
    assert!(
        String::from_utf8_lossy(&out)
            .contains("api.user.add_user_to_group_syncables.not_ldap_user.app_error")
    );

    // A SAML user is refused while `SamlSettings.EnableSyncWithLdap` is off (the stock value).
    assert!(set_user_auth_service(&plain.id, "saml").await);
    invalidate_go_caches(&client, &admin).await;
    let out = both(
        &client,
        STACK,
        post.clone(),
        Some(&admin),
        &path(&plain.id),
        None,
        &[],
        400,
    )
    .await;
    assert!(String::from_utf8_lossy(&out).contains("not_ldap_user"));

    // An LDAP user passes the gate; the memberships are Go's (`CreateDefaultMemberships`).
    assert!(set_user_auth_service(&plain.id, "ldap").await);
    invalidate_go_caches(&client, &admin).await;
    assert_eq!(
        both_forwarded(&client, post, Some(&admin), &path(&plain.id), None, &[]).await,
        200
    );

    assert!(set_user_auth_service(&plain.id, "").await);
    invalidate_go_caches(&client, &admin).await;
    delete_plain_user(&client, &admin, &plain.id).await;
}
