//! Port of `model/user_access_token.go` — personal access tokens.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_empty_str;
use crate::utils::{AppError, AppResult, ID_LENGTH, get_millis, is_valid_id, new_id};

/// The `Description` cap, inline in Go's `IsValid`.
pub const USER_ACCESS_TOKEN_DESCRIPTION_MAX_LENGTH: usize = 255;

/// Port of `model.NonCompliantUserAccessTokenResult` (user_access_token.go:11) — how many tokens
/// a maximum-lifetime sweep previewed or revoked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NonCompliantUserAccessTokenResult {
    #[serde(rename = "count")]
    pub count: i64,
}

/// Port of `model.UserAccessTokenSearch` (user_access_token_search.go:6) — the body of
/// `POST /users/tokens/search`.
///
/// One field, and the handler rejects an empty `term` before the store sees it. Note what the
/// store then does with it: the term is escaped and bound into three `LIKE`s with no `%` added,
/// so the "search" is an **equality** and a wildcard the caller writes matches itself. See
/// `mm_store::UserAccessTokenStore::search`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserAccessTokenSearch {
    #[serde(rename = "term")]
    pub term: String,
}

/// Port of `model.UserAccessToken` (user_access_token.go:15).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserAccessToken {
    #[serde(rename = "id")]
    pub id: String,

    /// The secret itself. **`omitempty`** — the only field here that is ever dropped, so a
    /// listing that clears it does not leak a key, while a creation response carries it.
    #[serde(rename = "token", skip_serializing_if = "is_empty_str")]
    pub token: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "is_active")]
    pub is_active: bool,

    /// Epoch milliseconds. **`0` means never expires**, for tokens minted before expiry existed.
    #[serde(rename = "expires_at")]
    pub expires_at: i64,

    /// `json:"-"` — when the owner was last warned about approaching expiry. The dedup key is
    /// derived as `expires_at - last_notified_at`, so the *moment* is stored rather than the
    /// warning bucket: the column stays correct if the buckets change. Never exposed over the
    /// API.
    #[serde(skip)]
    pub last_notified_at: Option<i64>,
}

impl UserAccessToken {
    /// Port of `(*UserAccessToken).IsValid` (user_access_token.go:37).
    ///
    /// Note it rejects only a **negative** `expires_at`; a past one is valid but expired — see
    /// [`UserAccessToken::is_expired`].
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("id"));
        }

        if self.token.len() != ID_LENGTH {
            return Err(err("token"));
        }

        if !is_valid_id(&self.user_id) {
            return Err(err("user_id"));
        }

        if self.description.len() > USER_ACCESS_TOKEN_DESCRIPTION_MAX_LENGTH {
            return Err(err("description"));
        }

        if self.expires_at < 0 {
            return Err(err("expires_at"));
        }

        Ok(())
    }

    /// Port of `(*UserAccessToken).PreSave` (user_access_token.go:61).
    ///
    /// **Overwrites the id unconditionally** — unlike every other `PreSave` in the package, which
    /// fills it only when empty. Calling this on a stored token re-keys it.
    pub fn pre_save(&mut self) {
        self.id = new_id();
        self.is_active = true;
    }

    /// Port of `(*UserAccessToken).IsExpired` (user_access_token.go:69).
    ///
    /// The comparison is `>=`, so a token expires **at** its expiry millisecond, not after it.
    pub fn is_expired(&self) -> bool {
        if self.expires_at <= 0 {
            return false;
        }
        get_millis() >= self.expires_at
    }
}

fn err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "UserAccessToken.IsValid",
        format!("model.user_access_token.is_valid.{field}.app_error"),
        None,
        "",
        400,
    ))
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
    fn non_compliant_user_access_token_result_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            NonCompliantUserAccessTokenResult,
            "non_compliant_user_access_token_result"
        );
    }
    #[test]
    fn user_access_token_search_round_trips_the_fixture() {
        assert_fixture_round_trips!(UserAccessTokenSearch, "user_access_token_search");
    }
    #[test]
    fn user_access_token_round_trips_the_fixture() {
        assert_fixture_round_trips!(UserAccessToken, "user_access_token");
    }
}
