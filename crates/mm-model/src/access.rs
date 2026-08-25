//! Port of `model/access.go` — the OAuth 2 access-token grant and response.

use serde::{Deserialize, Serialize};

use crate::authorize::validate_resource_parameter;
use crate::serde_helpers::is_empty_str;
use crate::utils::{AppError, AppResult, ID_LENGTH, get_millis, is_valid_http_url};

/// Port of `model.AccessTokenGrantType` (access.go:11) — the `grant_type` value for the
/// authorization-code exchange. Note it is the same string as
/// `oauth_metadata::GRANT_TYPE_AUTHORIZATION_CODE`, declared twice in the Go tree.
pub const ACCESS_TOKEN_GRANT_TYPE: &str = "authorization_code";
/// Port of `model.AccessTokenType` (access.go:12) — the `token_type` this server issues.
pub const ACCESS_TOKEN_TYPE: &str = "bearer";
/// Port of `model.RefreshTokenGrantType` (access.go:13).
pub const REFRESH_TOKEN_GRANT_TYPE: &str = "refresh_token";

/// The `RedirectUri` cap, inline in Go's `IsValid`.
pub const ACCESS_DATA_REDIRECT_URI_MAX_LENGTH: usize = 256;

/// Port of `model.AccessData` (access.go:16) — the stored grant.
///
/// **Every field is on the wire with no `omitempty`**, including `token` and `refresh_token`.
/// This type is a database row, not a response: the response shape is [`AccessResponse`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessData {
    #[serde(rename = "client_id")]
    pub client_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "token")]
    pub token: String,

    #[serde(rename = "refresh_token")]
    pub refresh_token: String,

    #[serde(rename = "redirect_uri")]
    pub redirect_uri: String,

    /// Epoch milliseconds; `0` or negative means it never expires.
    #[serde(rename = "expires_at")]
    pub expires_at: i64,

    #[serde(rename = "scope")]
    pub scope: String,

    /// RFC 8707 resource indicator.
    #[serde(rename = "audience")]
    pub audience: String,
}

impl AccessData {
    /// Port of `(*AccessData).IsValid` (access.go:38).
    ///
    /// The id checks are **length bounds, not `IsValidId`**: `client_id` and `user_id` need only
    /// be 1..=26 bytes, so a 26-character string of any bytes passes. `refresh_token` is bounded
    /// but **may be empty** — a grant without one is valid.
    pub fn is_valid(&self) -> AppResult {
        if self.client_id.is_empty() || self.client_id.len() > ID_LENGTH {
            return Err(err("client_id"));
        }

        if self.user_id.is_empty() || self.user_id.len() > ID_LENGTH {
            return Err(err("user_id"));
        }

        if self.token.len() != ID_LENGTH {
            return Err(err("access_token"));
        }

        if self.refresh_token.len() > ID_LENGTH {
            return Err(err("refresh_token"));
        }

        if self.redirect_uri.is_empty()
            || self.redirect_uri.len() > ACCESS_DATA_REDIRECT_URI_MAX_LENGTH
            || !is_valid_http_url(&self.redirect_uri)
        {
            return Err(err("redirect_uri"));
        }

        if !self.audience.is_empty() {
            // Returns the resource validator's own error, not one of this file's.
            validate_resource_parameter(&self.audience, &self.client_id, "AccessData.IsValid")?;
        }

        Ok(())
    }

    /// Port of `(*AccessData).IsExpired` (access.go:70).
    ///
    /// Strictly `>`, so a token is still valid **at** its expiry millisecond — the opposite of
    /// `UserAccessToken::is_expired`, which uses `>=`.
    pub fn is_expired(&self) -> bool {
        if self.expires_at <= 0 {
            return false;
        }
        get_millis() > self.expires_at
    }
}

fn err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "AccessData.IsValid",
        format!("model.access.is_valid.{field}.app_error"),
        None,
        "",
        400,
    ))
}

/// Port of `model.AccessResponse` (access.go:27) — the token endpoint's response body.
///
/// **`expires_in` is seconds and an `int32`**, not milliseconds like everything else in this
/// crate — RFC 6749 specifies it that way. `audience` is the only `omitempty` field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessResponse {
    #[serde(rename = "access_token")]
    pub access_token: String,

    /// Always [`ACCESS_TOKEN_TYPE`] in practice.
    #[serde(rename = "token_type")]
    pub token_type: String,

    /// **Seconds.**
    #[serde(rename = "expires_in")]
    pub expires_in_seconds: i32,

    #[serde(rename = "scope")]
    pub scope: String,

    #[serde(rename = "refresh_token")]
    pub refresh_token: String,

    #[serde(rename = "id_token")]
    pub id_token: String,

    #[serde(rename = "audience", skip_serializing_if = "is_empty_str")]
    pub audience: String,
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
    fn access_data_round_trips_the_fixture() {
        assert_fixture_round_trips!(AccessData, "access_data");
    }
    #[test]
    fn access_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(AccessResponse, "access_response");
    }
}
