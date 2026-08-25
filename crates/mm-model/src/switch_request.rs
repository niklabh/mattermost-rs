//! Port of `model/switch_request.go` — the body of the change-auth-method route.
//!
//! # The four predicates are not symmetric
//!
//! `EmailToOAuth` and `OAuthToEmail` treat **SAML and LDAP-adjacent services as "OAuth"**: the
//! set is `saml`, `gitlab`, `google`, `office365`, `openid`. LDAP is deliberately *not* in it and
//! gets its own pair. So a `saml → ldap` request satisfies none of the four and is rejected by
//! the caller rather than here — this file has no `IsValid`.

use serde::{Deserialize, Serialize};

use crate::user::USER_AUTH_SERVICE_EMAIL;
use crate::user::external::{
    SERVICE_GITLAB, SERVICE_GOOGLE, SERVICE_OFFICE365, SERVICE_OPENID, USER_AUTH_SERVICE_LDAP,
    USER_AUTH_SERVICE_SAML,
};

/// Port of `model.SwitchRequest` (switch_request.go:3).
///
/// **Three secrets on the wire with no `omitempty` and no `json:"-"`** — `password`,
/// `new_password` and `mfa_code` are always emitted if this type is ever serialised outbound.
/// `Auditable` deliberately lists only four of the seven fields; auditing is [D-028] and is not
/// ported here, but the omission is the point: the audit log must never carry these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SwitchRequest {
    #[serde(rename = "current_service")]
    pub current_service: String,

    #[serde(rename = "new_service")]
    pub new_service: String,

    #[serde(rename = "email")]
    pub email: String,

    #[serde(rename = "password")]
    pub password: String,

    #[serde(rename = "new_password")]
    pub new_password: String,

    #[serde(rename = "mfa_code")]
    pub mfa_code: String,

    /// Tagged **`ldap_id`**, while the field is `LdapLoginId` and `Auditable` calls it
    /// `ldap_login_id`. Three names for one value.
    #[serde(rename = "ldap_id")]
    pub ldap_login_id: String,
}

/// The five services the two OAuth predicates accept. `saml` is in the list; `ldap` is not.
fn is_oauth_service(service: &str) -> bool {
    matches!(
        service,
        USER_AUTH_SERVICE_SAML
            | SERVICE_GITLAB
            | SERVICE_GOOGLE
            | SERVICE_OFFICE365
            | SERVICE_OPENID
    )
}

impl SwitchRequest {
    /// Port of `(*SwitchRequest).EmailToOAuth` (switch_request.go:25).
    pub fn email_to_oauth(&self) -> bool {
        self.current_service == USER_AUTH_SERVICE_EMAIL && is_oauth_service(&self.new_service)
    }

    /// Port of `(*SwitchRequest).OAuthToEmail` (switch_request.go:34).
    pub fn oauth_to_email(&self) -> bool {
        is_oauth_service(&self.current_service) && self.new_service == USER_AUTH_SERVICE_EMAIL
    }

    /// Port of `(*SwitchRequest).EmailToLdap` (switch_request.go:43).
    pub fn email_to_ldap(&self) -> bool {
        self.current_service == USER_AUTH_SERVICE_EMAIL
            && self.new_service == USER_AUTH_SERVICE_LDAP
    }

    /// Port of `(*SwitchRequest).LdapToEmail` (switch_request.go:47).
    pub fn ldap_to_email(&self) -> bool {
        self.current_service == USER_AUTH_SERVICE_LDAP
            && self.new_service == USER_AUTH_SERVICE_EMAIL
    }
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
    fn switch_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(SwitchRequest, "switch_request");
    }
}
