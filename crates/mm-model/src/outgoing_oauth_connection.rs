//! Port of `model/outgoing_oauth_connection.go` — credentials the server uses to authenticate
//! *itself* to a third-party OAuth provider, so a slash command or outgoing webhook can carry a
//! bearer token.
//!
//! # `Patch` merges non-empty fields, which means it cannot clear one
//!
//! Every branch is `if conn.X != ""` (or `!= nil` for the two pointers), so a patch can set or
//! change a field but never blank it. That is the opposite of `PropertyFieldPatch`, whose `null`
//! deletes.
//!
//! # `HasValidGrantType` dereferences after checking — and Go still panics
//!
//! The password branch checks `CredentialsUsername == nil || CredentialsPassword == nil` and
//! returns; the **next** statement dereferences both. That is safe as written, but only because
//! the guard returns. Reproduced with pattern matching, which makes the dependency explicit.

use serde::{Deserialize, Serialize};

use crate::go_url::Values;
use crate::serde_helpers::{is_empty_str, is_none};
use crate::utils::{
    AppError, AppResult, StringArray, etag, get_millis, is_valid_http_url, is_valid_id, new_id,
};

/// Port of `model.OutgoingOAuthConnectionGrantTypeClientCredentials`
/// (outgoing_oauth_connection.go:19).
pub const OUTGOING_OAUTH_CONNECTION_GRANT_TYPE_CLIENT_CREDENTIALS: &str = "client_credentials";
/// Port of `model.OutgoingOAuthConnectionGrantTypePassword` (outgoing_oauth_connection.go:20).
pub const OUTGOING_OAUTH_CONNECTION_GRANT_TYPE_PASSWORD: &str = "password";

/// Port of `defaultGetConnectionsLimit` (outgoing_oauth_connection.go:22). Unexported in Go.
pub const DEFAULT_GET_CONNECTIONS_LIMIT: i64 = 50;

/// The inline caps in `IsValid`, all measured in **runes**.
pub const OUTGOING_OAUTH_CONNECTION_NAME_MAX_RUNES: usize = 64;
pub const OUTGOING_OAUTH_CONNECTION_CLIENT_ID_MAX_RUNES: usize = 255;
pub const OUTGOING_OAUTH_CONNECTION_CLIENT_SECRET_MAX_RUNES: usize = 255;
/// **256**, one more than the two secrets above.
pub const OUTGOING_OAUTH_CONNECTION_TOKEN_URL_MAX_RUNES: usize = 256;

/// Port of `(OutgoingOAuthConnectionGrantType).IsValid` (outgoing_oauth_connection.go:14).
pub fn is_valid_outgoing_oauth_connection_grant_type(grant_type: &str) -> bool {
    matches!(
        grant_type,
        OUTGOING_OAUTH_CONNECTION_GRANT_TYPE_CLIENT_CREDENTIALS
            | OUTGOING_OAUTH_CONNECTION_GRANT_TYPE_PASSWORD
    )
}

/// Port of `model.OutgoingOAuthConnection` (outgoing_oauth_connection.go:26).
///
/// **`client_secret` and `credentials_password` are on the wire** — they carry `omitempty`, not
/// `json:"-"`, so it is [`OutgoingOAuthConnection::sanitize`] that keeps them out of a response,
/// not the tag. A handler that forgets to call it leaks the secret.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutgoingOAuthConnection {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "creator_id")]
    pub creator_id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "client_id", skip_serializing_if = "is_empty_str")]
    pub client_id: String,

    /// Cleared by `sanitize`, not by the tag.
    #[serde(rename = "client_secret", skip_serializing_if = "is_empty_str")]
    pub client_secret: String,

    /// Required, with the password, for the `password` grant type.
    #[serde(rename = "credentials_username", skip_serializing_if = "is_none")]
    pub credentials_username: Option<String>,

    /// Cleared by `sanitize`.
    #[serde(rename = "credentials_password", skip_serializing_if = "is_none")]
    pub credentials_password: Option<String>,

    #[serde(rename = "oauth_token_url")]
    pub oauth_token_url: String,

    #[serde(rename = "grant_type")]
    pub grant_type: String,

    /// The URLs this connection's token may be used against. **At least one is required**, and
    /// each must be a valid HTTP URL — this is the allow-list that stops a token minted for one
    /// third party being sent to another.
    #[serde(rename = "audiences")]
    pub audiences: Option<StringArray>,
}

impl OutgoingOAuthConnection {
    fn audiences_slice(&self) -> &[String] {
        self.audiences.as_deref().unwrap_or(&[])
    }

    /// Port of `(*OutgoingOAuthConnection).Sanitize` (outgoing_oauth_connection.go:53).
    ///
    /// Clears **two** fields. Note `client_id` and `credentials_username` survive — only the
    /// secrets go.
    pub fn sanitize(&mut self) {
        self.client_secret.clear();
        self.credentials_password = None;
    }

    /// Port of `(*OutgoingOAuthConnection).Patch` (outgoing_oauth_connection.go:59).
    pub fn patch(&mut self, conn: &OutgoingOAuthConnection) {
        if !conn.name.is_empty() {
            self.name = conn.name.clone();
        }
        if !conn.client_id.is_empty() {
            self.client_id = conn.client_id.clone();
        }
        if !conn.client_secret.is_empty() {
            self.client_secret = conn.client_secret.clone();
        }
        if !conn.oauth_token_url.is_empty() {
            self.oauth_token_url = conn.oauth_token_url.clone();
        }
        if !conn.grant_type.is_empty() {
            self.grant_type = conn.grant_type.clone();
        }
        if !conn.audiences_slice().is_empty() {
            self.audiences = conn.audiences.clone();
        }
        if conn.credentials_username.is_some() {
            self.credentials_username = conn.credentials_username.clone();
        }
        if conn.credentials_password.is_some() {
            self.credentials_password = conn.credentials_password.clone();
        }
    }

    /// Port of `(*OutgoingOAuthConnection).IsValid` (outgoing_oauth_connection.go:88).
    ///
    /// The error ids end in **`.error`**, not `.app_error` — the only validator in the package
    /// that does. Only the audience branch carries i18n params, under the key `Url`.
    pub fn is_valid(&self) -> AppResult {
        let details = || format!("id={}", self.id);

        if !is_valid_id(&self.id) {
            return Err(err("id", String::new(), None));
        }

        if self.create_at == 0 {
            return Err(err("create_at", details(), None));
        }

        if self.update_at == 0 {
            return Err(err("update_at", details(), None));
        }

        if !is_valid_id(&self.creator_id) {
            return Err(err("creator_id", details(), None));
        }

        if self.name.is_empty()
            || self.name.chars().count() > OUTGOING_OAUTH_CONNECTION_NAME_MAX_RUNES
        {
            return Err(err("name", details(), None));
        }

        if self.client_id.is_empty()
            || self.client_id.chars().count() > OUTGOING_OAUTH_CONNECTION_CLIENT_ID_MAX_RUNES
        {
            return Err(err("client_id", details(), None));
        }

        if self.client_secret.is_empty()
            || self.client_secret.chars().count()
                > OUTGOING_OAUTH_CONNECTION_CLIENT_SECRET_MAX_RUNES
        {
            return Err(err("client_secret", details(), None));
        }

        if !is_valid_http_url(&self.oauth_token_url)
            || self.oauth_token_url.chars().count() > OUTGOING_OAUTH_CONNECTION_TOKEN_URL_MAX_RUNES
        {
            return Err(err("oauth_token_url", details(), None));
        }

        self.has_valid_grant_type()?;

        if self.audiences_slice().is_empty() {
            return Err(err("audience.empty", details(), None));
        }

        for audience in self.audiences_slice() {
            if !is_valid_http_url(audience) {
                return Err(err("audience", details(), Some(("Url", audience.clone()))));
            }
        }

        Ok(())
    }

    /// Port of `(*OutgoingOAuthConnection).HasValidGrantType` (outgoing_oauth_connection.go:139).
    ///
    /// The `password` grant needs both credentials **present and non-empty**; Go checks those as
    /// two separate branches returning the same error id.
    pub fn has_valid_grant_type(&self) -> AppResult {
        if !is_valid_outgoing_oauth_connection_grant_type(&self.grant_type) {
            return Err(err("grant_type", format!("id={}", self.id), None));
        }

        if self.grant_type == OUTGOING_OAUTH_CONNECTION_GRANT_TYPE_PASSWORD {
            let credentials_present = matches!(
                (&self.credentials_username, &self.credentials_password),
                (Some(username), Some(password)) if !username.is_empty() && !password.is_empty()
            );
            if !credentials_present {
                return Err(err("password_credentials", format!("id={}", self.id), None));
            }
        }

        Ok(())
    }

    /// Port of `(*OutgoingOAuthConnection).PreSave` (outgoing_oauth_connection.go:156) —
    /// `create_at` is set unconditionally.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        self.create_at = get_millis();
        self.update_at = self.create_at;
    }

    /// Port of `(*OutgoingOAuthConnection).PreUpdate` (outgoing_oauth_connection.go:166).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
    }

    /// Port of `(*OutgoingOAuthConnection).Etag` (outgoing_oauth_connection.go:171).
    pub fn etag(&self) -> String {
        etag(&[&self.id, &self.update_at])
    }
}

fn err(suffix: &str, details: String, param: Option<(&str, String)>) -> Box<AppError> {
    let params = param.map(|(key, value)| {
        let mut params = std::collections::HashMap::new();
        params.insert(key.to_string(), serde_json::Value::String(value));
        params
    });
    Box::new(AppError::new(
        "OutgoingOAuthConnection.IsValid",
        format!("model.outgoing_oauth_connection.is_valid.{suffix}.error"),
        params,
        details,
        400,
    ))
}

/// Port of `model.OutgoingOAuthConnectionGetConnectionsFilter`
/// (outgoing_oauth_connection.go:176). No `json:` tags — it becomes a query string.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutgoingOAuthConnectionGetConnectionsFilter {
    /// Keyset cursor.
    pub offset_id: String,
    pub limit: i64,
    pub audience: String,

    /// **Not a filter.** Go's comment is explicit: it exists so the caller's permission on that
    /// team can be checked before the connections are used by a slash command or outgoing
    /// webhook.
    pub team_id: String,
}

impl OutgoingOAuthConnectionGetConnectionsFilter {
    /// Port of `(*OutgoingOAuthConnectionGetConnectionsFilter).SetDefaults`
    /// (outgoing_oauth_connection.go:188).
    ///
    /// Only a **zero** limit is replaced — a negative one is left alone and reaches the store.
    pub fn set_defaults(&mut self) {
        if self.limit == 0 {
            self.limit = DEFAULT_GET_CONNECTIONS_LIMIT;
        }
    }

    /// Port of `(*OutgoingOAuthConnectionGetConnectionsFilter).ToURLValues`
    /// (outgoing_oauth_connection.go:195) — every parameter is conditional, so an empty filter
    /// produces an empty query.
    pub fn to_url_values(&self) -> Values {
        let mut v = Values::new();

        if self.limit > 0 {
            v.set("limit", &self.limit.to_string());
        }
        if !self.offset_id.is_empty() {
            v.set("offset_id", &self.offset_id);
        }
        if !self.audience.is_empty() {
            v.set("audience", &self.audience);
        }
        if !self.team_id.is_empty() {
            v.set("team_id", &self.team_id);
        }

        v
    }
}

/// Port of `model.OutgoingOAuthConnectionToken` (outgoing_oauth_connection.go:218) — the token
/// the provider returned.
///
/// **No `json:` tags**, so if it were marshalled the keys would be `AccessToken` and `TokenType`.
/// It is not: it is consumed immediately by [`OutgoingOAuthConnectionToken::as_header_value`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutgoingOAuthConnectionToken {
    pub access_token: String,
    pub token_type: String,
}

impl OutgoingOAuthConnectionToken {
    /// Port of `(*OutgoingOAuthConnectionToken).AsHeaderValue` (outgoing_oauth_connection.go:223).
    ///
    /// `"<token_type> <access_token>"` — the type comes from the provider verbatim, so it is
    /// whatever case the provider used (`Bearer`, `bearer`), not normalised.
    pub fn as_header_value(&self) -> String {
        format!("{} {}", self.token_type, self.access_token)
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
    fn outgoing_o_auth_connection_round_trips_the_fixture() {
        assert_fixture_round_trips!(OutgoingOAuthConnection, "outgoing_o_auth_connection");
    }
}
