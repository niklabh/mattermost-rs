//! Port of `model/oauth_metadata.go` — RFC 8414 authorization-server metadata.

use serde::{Deserialize, Serialize};

use crate::authorize::PKCE_CODE_CHALLENGE_METHOD_S256;
use crate::go_url::go_parse;
use crate::serde_helpers::{is_empty_str, is_none_or_empty_vec};

/// Port of `model.AuthorizationServerMetadata` (oauth_metadata.go:5).
///
/// **`response_types_supported` is the one list without `omitempty`** — RFC 8414 makes it
/// required, so it is emitted even when empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthorizationServerMetadata {
    #[serde(rename = "issuer")]
    pub issuer: String,

    #[serde(
        rename = "authorization_endpoint",
        skip_serializing_if = "is_empty_str"
    )]
    pub authorization_endpoint: String,

    #[serde(rename = "token_endpoint", skip_serializing_if = "is_empty_str")]
    pub token_endpoint: String,

    #[serde(rename = "response_types_supported")]
    pub response_types_supported: Option<Vec<String>>,

    #[serde(rename = "registration_endpoint", skip_serializing_if = "is_empty_str")]
    pub registration_endpoint: String,

    #[serde(
        rename = "scopes_supported",
        skip_serializing_if = "is_none_or_empty_vec"
    )]
    pub scopes_supported: Option<Vec<String>>,

    #[serde(
        rename = "grant_types_supported",
        skip_serializing_if = "is_none_or_empty_vec"
    )]
    pub grant_types_supported: Option<Vec<String>>,

    #[serde(
        rename = "token_endpoint_auth_methods_supported",
        skip_serializing_if = "is_none_or_empty_vec"
    )]
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,

    #[serde(
        rename = "code_challenge_methods_supported",
        skip_serializing_if = "is_none_or_empty_vec"
    )]
    pub code_challenge_methods_supported: Option<Vec<String>>,
}

pub const GRANT_TYPE_AUTHORIZATION_CODE: &str = "authorization_code";
pub const GRANT_TYPE_REFRESH_TOKEN: &str = "refresh_token";

pub const RESPONSE_TYPE_CODE: &str = "code";

/// Public clients, which authenticate with PKCE instead of a secret.
pub const CLIENT_AUTH_METHOD_NONE: &str = "none";
/// Confidential clients.
pub const CLIENT_AUTH_METHOD_CLIENT_SECRET_POST: &str = "client_secret_post";

/// The only scope this server issues.
pub const SCOPE_USER: &str = "user";

pub const OAUTH_AUTHORIZE_ENDPOINT: &str = "/oauth/authorize";
pub const OAUTH_ACCESS_TOKEN_ENDPOINT: &str = "/oauth/access_token";
pub const OAUTH_DEAUTHORIZE_ENDPOINT: &str = "/oauth/deauthorize";
pub const OAUTH_APPS_REGISTER_ENDPOINT: &str = "/api/v4/oauth/apps/register";
/// The well-known path this metadata is served from.
pub const OAUTH_METADATA_ENDPOINT: &str = "/.well-known/oauth-authorization-server";

/// Port of `model.GetDefaultMetadata` (oauth_metadata.go:38).
///
/// Note what is **not** advertised: `registration_endpoint` is left empty even though
/// [`OAUTH_APPS_REGISTER_ENDPOINT`] exists as a constant, so dynamic client registration is not
/// announced by default.
///
/// Go builds the two endpoints with `url.JoinPath`, which the `go_url` shim does not implement
/// ([D-049]); this joins with a single `/` after trimming, which agrees with `JoinPath` for every
/// input that reaches it — a site URL and an absolute, already-clean path constant.
pub fn get_default_metadata(site_url: &str) -> Result<AuthorizationServerMetadata, MetadataError> {
    // `url.JoinPath` parses the base first and fails on a malformed one.
    go_parse(site_url).map_err(|_| MetadataError::BadSiteUrl)?;

    let join = |path: &str| {
        format!(
            "{}/{}",
            site_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    };

    Ok(AuthorizationServerMetadata {
        issuer: site_url.to_string(),
        authorization_endpoint: join(OAUTH_AUTHORIZE_ENDPOINT),
        token_endpoint: join(OAUTH_ACCESS_TOKEN_ENDPOINT),
        response_types_supported: Some(vec![RESPONSE_TYPE_CODE.to_string()]),
        registration_endpoint: String::new(),
        scopes_supported: Some(vec![SCOPE_USER.to_string()]),
        grant_types_supported: Some(vec![
            GRANT_TYPE_AUTHORIZATION_CODE.to_string(),
            GRANT_TYPE_REFRESH_TOKEN.to_string(),
        ]),
        token_endpoint_auth_methods_supported: Some(vec![
            CLIENT_AUTH_METHOD_NONE.to_string(),
            CLIENT_AUTH_METHOD_CLIENT_SECRET_POST.to_string(),
        ]),
        code_challenge_methods_supported: Some(vec![PKCE_CODE_CHALLENGE_METHOD_S256.to_string()]),
    })
}

/// The one failure `GetDefaultMetadata` can report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MetadataError {
    #[error("site URL is not a valid URL")]
    BadSiteUrl,
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn authorization_server_metadata_round_trips_the_fixture() {
        assert_fixture_round_trips!(AuthorizationServerMetadata, "authorization_server_metadata");
    }
}
