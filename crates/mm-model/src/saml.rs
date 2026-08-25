//! Port of `model/saml.go` — the SAML auth-service constants and the three JSON types.
//!
//! # The SAML metadata tree is not ported
//!
//! Two thirds of `saml.go` is `EntityDescriptor` and its twenty satellite types, and they carry
//! **`xml:` tags only** — no `json:` tag anywhere. Reproducing them needs an XML codec with
//! namespace-qualified attributes, `,chardata`, `,innerxml` and `xml.Name` embedding, none of
//! which this crate has a dependency for; `xml_helpers.go` is deferred for the same reason.
//! Nothing in the migrated routes reads them.
//!
//! What is ported is everything a client sees: the auth-service names, the certificate status and
//! the metadata response.

use serde::{Deserialize, Serialize};

/// Port of `model.UserAuthServiceSaml` (saml.go:12) — the value stored in `User.AuthService`.
pub const USER_AUTH_SERVICE_SAML: &str = "saml";
/// Port of `model.UserAuthServiceSamlText` (saml.go:13) — the **display** form, upper-case.
pub const USER_AUTH_SERVICE_SAML_TEXT: &str = "SAML";
/// Port of `model.UserAuthServiceIsSaml` (saml.go:14). A **camelCase query parameter**, not an
/// auth service — `isSaml`, alongside `isMobile` and `isOAuthUser`.
pub const USER_AUTH_SERVICE_IS_SAML: &str = "isSaml";
/// Port of `model.UserAuthServiceIsMobile` (saml.go:15).
pub const USER_AUTH_SERVICE_IS_MOBILE: &str = "isMobile";
/// Port of `model.UserAuthServiceIsOAuth` (saml.go:16) — the value is **`isOAuthUser`**, not
/// `isOAuth`.
pub const USER_AUTH_SERVICE_IS_OAUTH: &str = "isOAuthUser";

/// Port of `model.SamlAuthRequest` (saml.go:19). **No tags at all** — it is built and consumed
/// inside one request, so the wire keys would be the Go field names if it were ever marshalled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SamlAuthRequest {
    /// The `AuthnRequest` document, base64-encoded and deflate-compressed.
    pub base64_auth_request: String,
    /// The IdP endpoint to redirect to.
    pub url: String,
    /// Round-tripped through the IdP and back; carries where to send the user afterwards.
    pub relay_state: String,
}

/// Port of `model.SamlCertificateStatus` (saml.go:25) — which of the three files are present on
/// disk. Booleans only: nothing here reads the certificates themselves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SamlCertificateStatus {
    #[serde(rename = "idp_certificate_file")]
    pub idp_certificate_file: bool,

    #[serde(rename = "private_key_file")]
    pub private_key_file: bool,

    #[serde(rename = "public_certificate_file")]
    pub public_certificate_file: bool,
}

/// Port of `model.SamlMetadataResponse` (saml.go:31) — what the server extracted from an IdP's
/// metadata document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SamlMetadataResponse {
    #[serde(rename = "idp_descriptor_url")]
    pub idp_descriptor_url: String,

    #[serde(rename = "idp_url")]
    pub idp_url: String,

    /// PEM, without the armour lines.
    #[serde(rename = "idp_public_certificate")]
    pub idp_public_certificate: String,
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
    fn saml_certificate_status_round_trips_the_fixture() {
        assert_fixture_round_trips!(SamlCertificateStatus, "saml_certificate_status");
    }
    #[test]
    fn saml_metadata_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(SamlMetadataResponse, "saml_metadata_response");
    }
}
