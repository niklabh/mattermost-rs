//! Cross-server parity for `api4/ldap_local.go` and `InitSamlLocal` over the two unix sockets —
//! the seven local-mode pairs of `mm_api::local_auth_certs`.
//!
//! The local session is unrestricted, so every permission check passes and what is compared is
//! what comes after it: the licence gate (the socket belongs to the stack's unlicensed Go, so
//! `sync` and `test` are the 501 here), the body decoders, and the multipart parse. The two
//! removes and a well-formed add are forwarded over the socket, and the comparison then is that
//! both answers are Go's.

use super::super::common;
use super::super::common::local_socket::{
    assert_forwarded_body_is_gos, both_maybe_forwarded, both_with_body, go_socket, rust_socket,
    sockets_enabled,
};

const BOUNDARY: &str = "mmrslocalauthcertsboundary";

/// One request with an owned body and content type over one socket.
async fn send(
    socket: &std::path::Path,
    method: &str,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (u16, axum::http::HeaderMap, Vec<u8>) {
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("Host", "localhost")
        .header("Content-Type", content_type)
        .header("Content-Length", body.len().to_string())
        .body(axum::body::Body::from(body))
        .expect("request builds");
    let response = mm_api::local::send_over_unix(socket, request)
        .await
        .unwrap_or_else(|e| panic!("{method} {path} over {}: {e}", socket.display()));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads")
        .to_vec();
    (status, headers, body)
}

fn multipart(parts: &[(&str, &str)]) -> (String, Vec<u8>) {
    let mut body = Vec::new();
    for (name, content) in parts {
        body.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"cert.pem\"\r\n\r\n{content}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={BOUNDARY}"), body)
}

fn assert_pair((go, rs): ((u16, Vec<u8>), (u16, Vec<u8>)), expected: u16, id: &str, context: &str) {
    assert_eq!(
        go.0,
        expected,
        "{context}: Go {}",
        String::from_utf8_lossy(&go.1)
    );
    assert_eq!(
        rs.0,
        expected,
        "{context}: ours {}",
        String::from_utf8_lossy(&rs.1)
    );
    common::assert_error_bodies_match_except_known_gaps(&go.1, &rs.1, context);
    assert!(
        String::from_utf8_lossy(&go.1).contains(id),
        "{context}: {id}"
    );
}

#[tokio::test]
async fn the_ldap_and_saml_gates_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let _unlicensed = common::ACTIVE_LICENCE_ROW.read().await;

    for path in ["/api/v4/ldap/sync", "/api/v4/ldap/test"] {
        assert_pair(
            both_with_body("POST", path, "{}").await,
            501,
            "api.ldap_groups.license_error",
            path,
        );
    }
    let path = "/api/v4/ldap/migrateid";
    for body in ["{}", r#"{"toAttribute":""}"#, "null"] {
        assert_pair(
            both_with_body("POST", path, body).await,
            400,
            "api.context.invalid_body_param.app_error",
            body,
        );
    }
    assert_pair(
        both_with_body("POST", path, r#"{"toAttribute":"objectGUID"}"#).await,
        501,
        "api.ldap_groups.license_error",
        "migrateid",
    );

    let path = "/api/v4/saml/reset_auth_data";
    assert_pair(
        both_with_body("POST", path, "{}").await,
        501,
        "api.admin.saml.not_available.app_error",
        "reset {}",
    );
    for body in ["null", "[]", r#"{"user_ids":"x"}"#] {
        assert_pair(
            both_with_body("POST", path, body).await,
            400,
            "model.utils.decode_json.app_error",
            body,
        );
    }
}

#[tokio::test]
async fn the_ldap_certificate_pairs_over_the_socket() {
    if !sockets_enabled() {
        return;
    }
    let go = go_socket().expect("checked by sockets_enabled");
    let rust = rust_socket().expect("checked by sockets_enabled");
    let (form_type, no_part) = multipart(&[("other", "x")]);
    let (_, one_part) = multipart(&[(
        "certificate",
        "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
    )]);

    for path in [
        "/api/v4/ldap/certificate/public",
        "/api/v4/ldap/certificate/private",
    ] {
        assert_pair(
            both_with_body("POST", path, "{}").await,
            400,
            "api.admin.add_certificate.parseform.app_error",
            path,
        );
        let (go_status, _, go_body) = send(&go, "POST", path, &form_type, no_part.clone()).await;
        let (rs_status, rs_headers, rs_body) =
            send(&rust, "POST", path, &form_type, no_part.clone()).await;
        assert_eq!(
            rs_headers
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust"),
            "{path}: the parse is ours"
        );
        assert_pair(
            ((go_status, go_body), (rs_status, rs_body)),
            400,
            "api.admin.add_certificate.no_file.app_error",
            path,
        );

        // A well-formed add is forwarded over the socket and written by Go; so is the remove.
        let (go_status, _, go_body) = send(&go, "POST", path, &form_type, one_part.clone()).await;
        let (rs_status, rs_headers, rs_body) =
            send(&rust, "POST", path, &form_type, one_part.clone()).await;
        // A response forwarded over the socket carries no `x-mmrs-served-by` at all — the
        // marker is the port proxy's — so the assertion is that it is not ours.
        assert_ne!(
            rs_headers
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust"),
            "{path}: the write is Go's"
        );
        assert_eq!((go_status, rs_status), (200, 200), "{path}: add");
        assert_eq!(go_body, rs_body);

        let (go, rs, served_here) = both_maybe_forwarded("DELETE", path).await;
        assert!(!served_here, "{path}: the remove is Go's");
        assert_eq!((go.0, rs.0), (200, 200), "{path}: remove");
        assert_forwarded_body_is_gos(&go.1, &rs.1, path);
    }
}
