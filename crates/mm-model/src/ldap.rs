//! Port of `model/ldap.go` — the LDAP auth-service constants and the diagnostic result shapes.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_empty_str;
use crate::utils::StringMap;

/// Port of `model.UserAuthServiceLdap` (ldap.go:4).
pub const USER_AUTH_SERVICE_LDAP: &str = "ldap";
/// Port of `model.LdapPublicCertificateName` (ldap.go:5).
pub const LDAP_PUBLIC_CERTIFICATE_NAME: &str = "ldap-public.crt";
/// Port of `model.LdapPrivateKeyName` (ldap.go:6).
pub const LDAP_PRIVATE_KEY_NAME: &str = "ldap-private.key";

/// Port of `model.LdapDiagnosticTestType` (ldap.go:10) — a `string` newtype.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LdapDiagnosticTestType(pub String);

impl LdapDiagnosticTestType {
    /// Port of `model.LdapDiagnosticTestTypeFilters` (ldap.go:13).
    pub const FILTERS: &'static str = "filters";
    /// Port of `model.LdapDiagnosticTestTypeAttributes` (ldap.go:14).
    pub const ATTRIBUTES: &'static str = "attributes";
    /// Port of `model.LdapDiagnosticTestTypeGroupAttributes` (ldap.go:15).
    pub const GROUP_ATTRIBUTES: &'static str = "group_attributes";

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Port of `(LdapDiagnosticTestType).IsValid` (ldap.go:19) — a closed set of three.
    pub fn is_valid(&self) -> bool {
        matches!(
            self.0.as_str(),
            Self::FILTERS | Self::ATTRIBUTES | Self::GROUP_ATTRIBUTES
        )
    }
}

impl From<&str> for LdapDiagnosticTestType {
    fn from(s: &str) -> Self {
        LdapDiagnosticTestType(s.to_string())
    }
}

/// Port of `model.LdapDiagnosticResult` (ldap.go:29).
///
/// Note the asymmetry in the two string fields at the end: `message` has `omitempty` and `error`
/// does **not**, so a successful diagnostic still writes `"error":""`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LdapDiagnosticResult {
    #[serde(rename = "test_name")]
    pub test_name: String,

    #[serde(rename = "test_value")]
    pub test_value: String,

    #[serde(rename = "total_count")]
    pub total_count: i64,

    /// Only meaningful for the `attributes` test.
    #[serde(rename = "entries_with_value")]
    pub entries_with_value: i64,

    #[serde(rename = "message", skip_serializing_if = "is_empty_str")]
    pub message: String,

    #[serde(rename = "error")]
    pub error: String,

    #[serde(rename = "sample_results")]
    pub sample_results: Vec<LdapSampleEntry>,
}

/// Port of `model.LdapSampleEntry` (ldap.go:39).
///
/// Every field except `dn` carries `omitempty`, so a group entry and a user entry are different
/// key sets rather than one shape with blanks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LdapSampleEntry {
    /// The distinguished name. The only always-present key.
    #[serde(rename = "dn")]
    pub dn: String,

    #[serde(rename = "username", skip_serializing_if = "is_empty_str")]
    pub username: String,

    #[serde(rename = "email", skip_serializing_if = "is_empty_str")]
    pub email: String,

    #[serde(rename = "first_name", skip_serializing_if = "is_empty_str")]
    pub first_name: String,

    #[serde(rename = "last_name", skip_serializing_if = "is_empty_str")]
    pub last_name: String,

    #[serde(rename = "id", skip_serializing_if = "is_empty_str")]
    pub id: String,

    /// Groups only.
    #[serde(rename = "display_name", skip_serializing_if = "is_empty_str")]
    pub display_name: String,

    #[serde(
        rename = "available_attributes",
        skip_serializing_if = "crate::serde_helpers::is_empty_map"
    )]
    pub available_attributes: StringMap,
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
    fn ldap_diagnostic_result_round_trips_the_fixture() {
        assert_fixture_round_trips!(LdapDiagnosticResult, "ldap_diagnostic_result");
    }
    #[test]
    fn ldap_sample_entry_round_trips_the_fixture() {
        assert_fixture_round_trips!(LdapSampleEntry, "ldap_sample_entry");
    }
}
