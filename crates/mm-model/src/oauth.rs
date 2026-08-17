//! Port of `server/public/model/oauth.go`.
//!
//! Three things here are not what the source reads like, all three measured against Go in
//! `fixtures/behaviour_oauth.json`:
//!
//! * the callback-URL length cap measures Go's **slice formatting**, `[a b c]`, not the URLs —
//!   see [`OAuthApp::is_valid`];
//! * `Name` is capped in **bytes** and `Description` in **runes**, in the same function;
//! * [`OAuthApp::auditable`] emits a key with a trailing colon, `"callback_urls:"`.

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::utils::{
    AppError, AppResult, StringArray, StringInterface, etag, get_millis, is_valid_http_url,
    is_valid_id, new_id,
};

/// Constants borrowed from files this port has not reached. Pinned by the oracle, so a drift
/// upstream fails a test rather than passing silently.
pub mod external {
    /// access.go:11
    pub const ACCESS_TOKEN_GRANT_TYPE: &str = "authorization_code";
    /// access.go:13
    pub const REFRESH_TOKEN_GRANT_TYPE: &str = "refresh_token";
    /// oauth_metadata.go:26
    pub const CLIENT_AUTH_METHOD_NONE: &str = "none";
    /// oauth_metadata.go:27
    pub const CLIENT_AUTH_METHOD_CLIENT_SECRET_POST: &str = "client_secret_post";
    /// oauth_metadata.go:29
    pub const SCOPE_USER: &str = "user";
}
use external::*;

/// oauth.go:15
pub const OAUTH_ACTION_SIGNUP: &str = "signup";
/// oauth.go:16
pub const OAUTH_ACTION_LOGIN: &str = "login";
/// oauth.go:17
pub const OAUTH_ACTION_EMAIL_TO_SSO: &str = "email_to_sso";
/// oauth.go:18
pub const OAUTH_ACTION_SSO_TO_EMAIL: &str = "sso_to_email";
/// oauth.go:19
pub const OAUTH_ACTION_MOBILE: &str = "mobile";

// The caps, each named for the field it guards so the byte/rune distinction is visible at the
// point of use rather than buried in a literal.
const CLIENT_SECRET_MAX_BYTES: usize = 128;
const NAME_MAX_BYTES: usize = 64;
const CALLBACKS_RENDERED_MAX_BYTES: usize = 1024;
const HOMEPAGE_MAX_BYTES: usize = 256;
const DESCRIPTION_MAX_RUNES: usize = 512;
const ICON_URL_MAX_BYTES: usize = 512;
const MATTERMOST_APP_ID_MAX_BYTES: usize = 32;

fn is_false(value: &bool) -> bool {
    !*value
}

/// Port of `model.IntuneLoginRequest` (oauth.go:26).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntuneLoginRequest {
    #[serde(rename = "access_token")]
    pub access_token: String,
    #[serde(rename = "device_id")]
    pub device_id: String,
    /// The only `omitempty` field on this type.
    #[serde(
        rename = "voip_device_id",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub voip_device_id: String,
}

/// Port of `model.OAuthApp` (oauth.go:32).
///
/// Only `is_dynamically_registered` carries `omitempty`, so a zero-valued app emits twelve keys —
/// including `"callback_urls": null`, because a nil `StringArray` is not dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthApp {
    #[serde(rename = "id")]
    pub id: String,
    #[serde(rename = "creator_id")]
    pub creator_id: String,
    #[serde(rename = "create_at")]
    pub create_at: i64,
    #[serde(rename = "update_at")]
    pub update_at: i64,
    #[serde(rename = "client_secret")]
    pub client_secret: String,
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "description")]
    pub description: String,
    #[serde(rename = "icon_url")]
    pub icon_url: String,
    #[serde(rename = "callback_urls")]
    pub callback_urls: Option<StringArray>,
    #[serde(rename = "homepage")]
    pub homepage: String,
    #[serde(rename = "is_trusted")]
    pub is_trusted: bool,
    #[serde(rename = "mattermost_app_id")]
    pub mattermost_app_id: String,

    #[serde(
        rename = "is_dynamically_registered",
        default,
        skip_serializing_if = "is_false"
    )]
    pub is_dynamically_registered: bool,
}

/// Port of `model.OAuthAppRequest` (oauth.go:50) — "the request body for creating an OAuth app".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthAppRequest {
    #[serde(rename = "name")]
    pub name: String,
    #[serde(rename = "description")]
    pub description: String,
    #[serde(rename = "icon_url")]
    pub icon_url: String,
    #[serde(rename = "callback_urls")]
    pub callback_urls: Option<StringArray>,
    #[serde(rename = "homepage")]
    pub homepage: String,
    #[serde(rename = "is_trusted")]
    pub is_trusted: bool,
    #[serde(rename = "is_public")]
    pub is_public: bool,
}

/// Go's `fmt.Sprintf("%s", someStringSlice)`.
///
/// Renders as `[first second third]` — square brackets, single-space separators, no quotes and no
/// commas. A nil or empty slice is `[]`.
///
/// This exists because [`OAuthApp::is_valid`] caps the **rendered** length rather than the sum of
/// the entries, so the two brackets and the `n-1` separators count against the limit. It is also
/// not a parseable encoding: `["a b"]` and `["a", "b"]` both render as `[a b]`.
fn go_format_string_slice(values: &[String]) -> String {
    let mut out = String::with_capacity(2 + values.iter().map(|v| v.len() + 1).sum::<usize>());
    out.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(value);
    }
    out.push(']');
    out
}

impl OAuthApp {
    fn callbacks(&self) -> &[String] {
        self.callback_urls.as_deref().unwrap_or_default()
    }

    fn error(&self, id: &str, with_app_id: bool) -> Box<AppError> {
        Box::new(AppError::new(
            "OAuthApp.IsValid",
            id,
            None,
            if with_app_id {
                format!("app_id={}", self.id)
            } else {
                String::new()
            },
            400,
        ))
    }

    /// Port of `(*OAuthApp).IsValid` (oauth.go:78).
    ///
    /// # The callback cap measures a rendering, not the URLs
    ///
    /// Go writes `len(fmt.Sprintf("%s", a.CallbackUrls)) > 1024`. `CallbackUrls` is a `[]string`,
    /// and `%s` renders a slice as `[first second third]` — so the limit applies to that string,
    /// which is the sum of the entries **plus one separator between each plus two brackets**.
    /// Measured: one 28-byte URL renders to 30 bytes; two 24-byte URLs render to 51, not 48.
    /// Summing the entries would accept payloads Go rejects at the boundary.
    ///
    /// # `Name` counts bytes, `Description` counts runes
    ///
    /// `len(a.Name) > 64` against `utf8.RuneCountInString(a.Description) > 512`, in the same
    /// function. Every other cap here is `len`, i.e. bytes. Measured: a 33-character name of
    /// two-byte runes (66 bytes) is **rejected**, while a 512-character description of two-byte
    /// runes (1024 bytes) is **accepted**.
    ///
    /// # `is_dynamically_registered` exempts two checks
    ///
    /// The creator-id check and the empty-homepage check are both skipped for a dynamically
    /// registered app, so the same object's validity flips with that one bool.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            // The only branch that does NOT carry `app_id=` in its detail — because at this point
            // the id is known to be invalid.
            return Err(self.error("model.oauth.is_valid.app_id.app_error", false));
        }

        if self.create_at == 0 {
            return Err(self.error("model.oauth.is_valid.create_at.app_error", true));
        }

        if self.update_at == 0 {
            return Err(self.error("model.oauth.is_valid.update_at.app_error", true));
        }

        if !is_valid_id(&self.creator_id) && !self.is_dynamically_registered {
            return Err(self.error("model.oauth.is_valid.creator_id.app_error", true));
        }

        // Go's comment: "Validate client secret length if present" — an empty secret is a public
        // client and is allowed.
        if !self.client_secret.is_empty() && self.client_secret.len() > CLIENT_SECRET_MAX_BYTES {
            return Err(self.error("model.oauth.is_valid.client_secret.app_error", true));
        }

        if self.name.is_empty() || self.name.len() > NAME_MAX_BYTES {
            return Err(self.error("model.oauth.is_valid.name.app_error", true));
        }

        let callbacks = self.callbacks();
        if callbacks.is_empty()
            || go_format_string_slice(callbacks).len() > CALLBACKS_RENDERED_MAX_BYTES
        {
            return Err(self.error("model.oauth.is_valid.callback.app_error", true));
        }

        for callback in callbacks {
            if !is_valid_http_url(callback) {
                // Note: no `app_id=` detail on this one, unlike the length branch above it that
                // reports the same error id.
                return Err(self.error("model.oauth.is_valid.callback.app_error", false));
            }
        }

        if self.homepage.is_empty() && !self.is_dynamically_registered {
            return Err(self.error("model.oauth.is_valid.homepage.app_error", true));
        }

        if !self.homepage.is_empty()
            && (self.homepage.len() > HOMEPAGE_MAX_BYTES || !is_valid_http_url(&self.homepage))
        {
            return Err(self.error("model.oauth.is_valid.homepage.app_error", true));
        }

        // Runes here, bytes everywhere else in this function.
        if self.description.chars().count() > DESCRIPTION_MAX_RUNES {
            return Err(self.error("model.oauth.is_valid.description.app_error", true));
        }

        if !self.icon_url.is_empty()
            && (self.icon_url.len() > ICON_URL_MAX_BYTES || !is_valid_http_url(&self.icon_url))
        {
            return Err(self.error("model.oauth.is_valid.icon_url.app_error", true));
        }

        if self.mattermost_app_id.len() > MATTERMOST_APP_ID_MAX_BYTES {
            return Err(self.error("model.oauth.is_valid.mattermost_app_id.app_error", true));
        }

        Ok(())
    }

    /// Port of `(*OAuthApp).PreSave` (oauth.go:140).
    ///
    /// Generates an id when absent and stamps both timestamps. It does **not** generate a client
    /// secret — Go's comment says so explicitly, "callers must explicitly set ClientSecret if they
    /// want to create a confidential client", so an app saved without one is a *public* client by
    /// construction.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        self.create_at = get_millis();
        self.update_at = self.create_at;
    }

    /// Port of `(*OAuthApp).PreUpdate` (oauth.go:153).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
    }

    /// Port of `(*OAuthApp).Etag` (oauth.go:158).
    pub fn etag(&self) -> String {
        etag(&[&self.id, &self.update_at])
    }

    /// Port of `(*OAuthApp).Sanitize` (oauth.go:163) — "remove any private data from the app
    /// object". Clears the client secret and nothing else.
    pub fn sanitize(&mut self) {
        self.client_secret.clear();
    }

    /// Port of `(*OAuthApp).IsValidRedirectURL` (oauth.go:167).
    ///
    /// Exact membership: no prefix matching, no case folding, no trailing-slash tolerance. That
    /// strictness is the security property — a loose comparison here is an open redirect.
    pub fn is_valid_redirect_url(&self, url: &str) -> bool {
        self.callbacks().iter().any(|callback| callback == url)
    }

    /// Port of `(*OAuthApp).GetTokenEndpointAuthMethod` (oauth.go:173).
    ///
    /// The presence of a secret *is* the client type — there is no separate flag.
    pub fn get_token_endpoint_auth_method(&self) -> &'static str {
        if self.client_secret.is_empty() {
            CLIENT_AUTH_METHOD_NONE
        } else {
            CLIENT_AUTH_METHOD_CLIENT_SECRET_POST
        }
    }

    /// Port of `(*OAuthApp).IsPublicClient` (oauth.go:181).
    pub fn is_public_client(&self) -> bool {
        self.get_token_endpoint_auth_method() == CLIENT_AUTH_METHOD_NONE
    }

    /// Port of `(*OAuthApp).ValidateForGrantType` (oauth.go:186).
    pub fn validate_for_grant_type(
        &self,
        grant_type: &str,
        client_secret: &str,
        code_verifier: &str,
    ) -> AppResult {
        if self.is_public_client() {
            self.validate_public_client_grant(grant_type, client_secret, code_verifier)
        } else {
            self.validate_confidential_client_grant(client_secret)
        }
    }

    /// Port of `(*OAuthApp).validatePublicClientGrant` (oauth.go:194) — "OAuth 2.1 security
    /// requirements".
    ///
    /// Note the third check is conditional on the grant type: a public client using neither the
    /// authorization-code nor the refresh-token grant passes without PKCE, because nothing in the
    /// chain matches it.
    fn validate_public_client_grant(
        &self,
        grant_type: &str,
        client_secret: &str,
        code_verifier: &str,
    ) -> AppResult {
        if !client_secret.is_empty() {
            return Err(self.grant_error(
                "OAuthApp.validatePublicClientGrant",
                "model.oauth.validate_grant.public_client_secret.app_error",
                400,
            ));
        }

        if grant_type == REFRESH_TOKEN_GRANT_TYPE {
            return Err(self.grant_error(
                "OAuthApp.validatePublicClientGrant",
                "model.oauth.validate_grant.public_client_refresh_token.app_error",
                400,
            ));
        }

        if grant_type == ACCESS_TOKEN_GRANT_TYPE && code_verifier.is_empty() {
            return Err(self.grant_error(
                "OAuthApp.validatePublicClientGrant",
                "model.oauth.validate_grant.pkce_required.app_error",
                400,
            ));
        }

        Ok(())
    }

    /// Port of `(*OAuthApp).validateConfidentialClientGrant` (oauth.go:214).
    ///
    /// # The comparison must stay constant-time
    ///
    /// Go uses `subtle.ConstantTimeCompare`, which returns 0 when the lengths differ **or** the
    /// contents differ. A short-circuiting `==` would leak both the secret's length and how much
    /// of a guess matched, through timing — which is why this takes the `subtle` dependency
    /// rather than comparing directly. Note the grant type is not consulted at all: a confidential
    /// client presenting the right secret passes for any grant.
    ///
    /// This branch is the only one in the file answering **401** rather than 400.
    fn validate_confidential_client_grant(&self, client_secret: &str) -> AppResult {
        let matches: bool = self
            .client_secret
            .as_bytes()
            .ct_eq(client_secret.as_bytes())
            .into();

        if !matches {
            return Err(self.grant_error(
                "OAuthApp.validateConfidentialClientGrant",
                "model.oauth.validate_grant.credentials.app_error",
                401,
            ));
        }

        Ok(())
    }

    fn grant_error(&self, where_: &'static str, id: &str, status: i32) -> Box<AppError> {
        Box::new(AppError::new(
            where_,
            id,
            None,
            format!("app_id={}", self.id),
            status,
        ))
    }

    /// Port of `(*OAuthApp).Auditable` (oauth.go:60).
    ///
    /// # The callback key carries a trailing colon
    ///
    /// Go writes `"callback_urls:": a.CallbackUrls` — a typo, and audit consumers read that key.
    /// Reproduced, because correcting it would mean the two servers write different audit records
    /// for the same event.
    ///
    /// Note this map includes `token_endpoint_auth_method`, which is *derived* rather than stored,
    /// and deliberately omits `client_secret` — the point of an `Auditable` implementation.
    pub fn auditable(&self) -> StringInterface {
        let value = serde_json::json!({
            "id": self.id,
            "creator_id": self.creator_id,
            "create_at": self.create_at,
            "update_at": self.update_at,
            "name": self.name,
            "description": self.description,
            "icon_url": self.icon_url,
            // Not a typo here — a typo *there*. See the note above.
            "callback_urls:": self.callback_urls,
            "homepage": self.homepage,
            "is_trusted": self.is_trusted,
            "mattermost_app_id": self.mattermost_app_id,
            "token_endpoint_auth_method": self.get_token_endpoint_auth_method(),
            "is_dynamically_registered": self.is_dynamically_registered,
        });
        match value {
            serde_json::Value::Object(map) => map,
            _ => StringInterface::new(),
        }
    }
}

impl crate::audit_record::Auditable for OAuthApp {
    fn auditable(&self) -> StringInterface {
        OAuthApp::auditable(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_app_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/oauth_app.json");
        let app: OAuthApp = serde_json::from_str(raw).expect("fixture decodes");
        let ours: serde_json::Value = serde_json::to_value(&app).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("fixture is json");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn oauth_app_request_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/oauth_app_request.json");
        let request: OAuthAppRequest = serde_json::from_str(raw).expect("fixture decodes");
        let ours: serde_json::Value = serde_json::to_value(&request).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("fixture is json");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn intune_login_request_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/intune_login_request.json");
        let request: IntuneLoginRequest = serde_json::from_str(raw).expect("fixture decodes");
        let ours: serde_json::Value = serde_json::to_value(&request).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("fixture is json");
        assert_eq!(ours, theirs);
    }
}

/// Parity tests driven by `fixtures/behaviour_oauth.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_oauth.json")).unwrap()
    }

    fn valid_app() -> OAuthApp {
        OAuthApp {
            id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            creator_id: "aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            create_at: 1_600_000_000_000,
            update_at: 1_650_000_000_000,
            client_secret: "a-client-secret".to_owned(),
            name: "My OAuth App".to_owned(),
            description: "does oauth things".to_owned(),
            icon_url: "https://example.com/icon.png".to_owned(),
            callback_urls: Some(vec!["https://example.com/callback".to_owned()]),
            homepage: "https://example.com".to_owned(),
            is_trusted: true,
            mattermost_app_id: "mmapp".to_owned(),
            is_dynamically_registered: false,
        }
    }

    fn ascii(n: usize) -> String {
        "a".repeat(n)
    }

    fn multibyte(n: usize) -> String {
        "é".repeat(n)
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(c["OAuthActionSignup"], OAUTH_ACTION_SIGNUP);
        assert_eq!(c["OAuthActionLogin"], OAUTH_ACTION_LOGIN);
        assert_eq!(c["OAuthActionEmailToSSO"], OAUTH_ACTION_EMAIL_TO_SSO);
        assert_eq!(c["OAuthActionSSOToEmail"], OAUTH_ACTION_SSO_TO_EMAIL);
        assert_eq!(c["OAuthActionMobile"], OAUTH_ACTION_MOBILE);
        // The borrowed ones, so a drift in access.go or oauth_metadata.go fails here.
        assert_eq!(c["AccessTokenGrantType"], ACCESS_TOKEN_GRANT_TYPE);
        assert_eq!(c["RefreshTokenGrantType"], REFRESH_TOKEN_GRANT_TYPE);
        assert_eq!(c["ClientAuthMethodNone"], CLIENT_AUTH_METHOD_NONE);
        assert_eq!(
            c["ClientAuthMethodClientSecretPost"],
            CLIENT_AUTH_METHOD_CLIENT_SECRET_POST
        );
        assert_eq!(c["ScopeUser"], SCOPE_USER);
    }

    /// The rendering the callback cap actually measures.
    #[test]
    fn slice_formatting_matches_go() {
        for case in oracle()["callback_format"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let values: Vec<String> = match name {
                "nil" | "empty" => vec![],
                "one" => vec!["https://example.com/callback".to_owned()],
                "two" => vec![
                    "https://a.example.com/cb".to_owned(),
                    "https://b.example.com/cb".to_owned(),
                ],
                "three" => vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
                "entry_with_space" => vec!["a b".to_owned()],
                "empty_entry" => vec![String::new(), String::new()],
                other => panic!("unmapped: {other}"),
            };

            let rendered = go_format_string_slice(&values);
            assert_eq!(
                rendered,
                case["rendered"].as_str().unwrap(),
                "rendering mismatch for {name}"
            );
            assert_eq!(
                rendered.len(),
                case["length"].as_u64().unwrap() as usize,
                "length mismatch for {name}"
            );
            // And the point of measuring: it is not the sum of the entries.
            if !values.is_empty() {
                assert_ne!(
                    rendered.len(),
                    case["sum_of_entries"].as_u64().unwrap() as usize,
                    "{name}: the rendering must differ from the naive sum"
                );
            }
        }
    }

    #[test]
    fn wire_format_is_byte_exact() {
        for case in oracle()["wire"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let expected = case["json"].as_str().unwrap();
            let ours = match name {
                "app_request" => {
                    let v: OAuthAppRequest = serde_json::from_str(expected).unwrap();
                    serde_json::to_string(&v).unwrap()
                }
                n if n.starts_with("intune_login") => {
                    let v: IntuneLoginRequest = serde_json::from_str(expected).unwrap();
                    serde_json::to_string(&v).unwrap()
                }
                _ => {
                    let v: OAuthApp = serde_json::from_str(expected).unwrap();
                    serde_json::to_string(&v).unwrap()
                }
            };
            assert_eq!(ours, expected, "wire mismatch for {name}");
        }
    }

    #[test]
    fn is_valid_matches_go() {
        // Built from the *rendered* length, which is what the cap measures.
        let prefix = "https://e.com/";
        let at_cap = format!("{prefix}{}", ascii(1024 - 2 - prefix.len()));
        let over_cap = format!("{prefix}{}", ascii(1024 - 2 - prefix.len() + 1));

        for case in oracle()["is_valid"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut a = valid_app();
            match name {
                "valid" => {}
                "bad_id" => a.id = "nope".to_owned(),
                "zero_create_at" => a.create_at = 0,
                "zero_update_at" => a.update_at = 0,
                "bad_creator_id" => a.creator_id = "nope".to_owned(),
                "bad_creator_id_but_dynamic" => {
                    a.creator_id = "nope".to_owned();
                    a.is_dynamically_registered = true;
                }
                "client_secret_at_cap" => a.client_secret = ascii(128),
                "client_secret_over_cap" => a.client_secret = ascii(129),
                "empty_client_secret_is_allowed" => a.client_secret = String::new(),
                "empty_name" => a.name = String::new(),
                "name_at_cap" => a.name = ascii(64),
                "name_over_cap" => a.name = ascii(65),
                "name_multibyte_33_runes_66_bytes" => a.name = multibyte(33),
                "name_multibyte_32_runes_64_bytes" => a.name = multibyte(32),
                "no_callbacks" => a.callback_urls = Some(vec![]),
                "nil_callbacks" => a.callback_urls = None,
                "callbacks_rendering_at_cap" => a.callback_urls = Some(vec![at_cap.clone()]),
                "callbacks_rendering_over_cap" => a.callback_urls = Some(vec![over_cap.clone()]),
                "callback_not_a_url" => a.callback_urls = Some(vec!["not a url".to_owned()]),
                "second_callback_not_a_url" => {
                    a.callback_urls =
                        Some(vec!["https://ok.example.com".to_owned(), "nope".to_owned()])
                }
                "empty_homepage" => a.homepage = String::new(),
                "empty_homepage_but_dynamic" => {
                    a.homepage = String::new();
                    a.is_dynamically_registered = true;
                }
                "homepage_not_a_url" => a.homepage = "not a url".to_owned(),
                "homepage_over_cap" => a.homepage = format!("https://e.com/{}", ascii(256)),
                "description_512_multibyte_runes" => a.description = multibyte(512),
                "description_513_runes" => a.description = ascii(513),
                "empty_icon_url_is_allowed" => a.icon_url = String::new(),
                "icon_url_not_a_url" => a.icon_url = "not a url".to_owned(),
                "icon_url_over_cap" => a.icon_url = format!("https://e.com/{}", ascii(512)),
                "mattermost_app_id_at_cap" => a.mattermost_app_id = ascii(32),
                "mattermost_app_id_over_cap" => a.mattermost_app_id = ascii(33),
                other => panic!("unmapped corpus case: {other}"),
            }

            let got = a.is_valid();
            if case["ok"].as_bool().unwrap() {
                assert!(got.is_ok(), "{name}: expected ok, got {got:?}");
            } else {
                let err = got.expect_err(&format!("{name}: expected an error"));
                assert_eq!(err.id, case["id"].as_str().unwrap(), "{name}: id");
                assert_eq!(err.where_, case["where"].as_str().unwrap(), "{name}: where");
                assert_eq!(
                    err.status_code,
                    case["status"].as_i64().unwrap() as i32,
                    "{name}: status"
                );
                assert_eq!(
                    err.detailed_error,
                    case["detailed_error"].as_str().unwrap(),
                    "{name}: detailed_error — two branches omit the app_id"
                );
            }
        }
    }

    /// The byte/rune asymmetry, asserted as its own claim so it cannot be lost in the corpus.
    #[test]
    fn name_counts_bytes_and_description_counts_runes() {
        let mut byte_capped = valid_app();
        byte_capped.name = multibyte(33); // 66 bytes, 33 chars
        assert!(
            byte_capped.is_valid().is_err(),
            "33 two-byte runes is 66 bytes and must fail a 64-BYTE cap"
        );

        let mut rune_capped = valid_app();
        rune_capped.description = multibyte(512); // 1024 bytes, 512 chars
        assert!(
            rune_capped.is_valid().is_ok(),
            "512 two-byte runes is 1024 bytes and must pass a 512-RUNE cap"
        );
    }

    #[test]
    fn etag_matches_go() {
        for case in oracle()["etag"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let app = match name {
                "typical" => valid_app(),
                "zero" => OAuthApp::default(),
                other => panic!("unmapped: {other}"),
            };
            assert_eq!(
                app.etag(),
                case["etag"].as_str().unwrap(),
                "etag for {name}"
            );
        }
    }

    #[test]
    fn sanitize_matches_go() {
        let case = &oracle()["sanitize"][0];
        let mut app = valid_app();
        let before = app.clone();
        app.sanitize();

        assert_eq!(
            app.client_secret,
            case["client_secret_after"].as_str().unwrap()
        );
        assert_eq!(
            serde_json::to_string(&app).unwrap(),
            case["json_after"].as_str().unwrap()
        );
        assert_eq!(app.id, before.id);
        assert_eq!(app.callback_urls, before.callback_urls);
        assert_eq!(app.is_trusted, before.is_trusted);
    }

    #[test]
    fn is_valid_redirect_url_matches_go() {
        let mut app = valid_app();
        app.callback_urls = Some(vec![
            "https://a.example.com/cb".to_owned(),
            "https://b.example.com/cb".to_owned(),
        ]);

        for case in oracle()["redirect_url"].as_array().unwrap() {
            let url = case["url"].as_str().unwrap();
            assert_eq!(
                app.is_valid_redirect_url(url),
                case["valid"].as_bool().unwrap(),
                "redirect mismatch for {url:?}"
            );
        }
    }

    #[test]
    fn auth_method_matches_go() {
        for case in oracle()["auth_method"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut app = valid_app();
            if name == "without_secret" {
                app.client_secret = String::new();
            }
            assert_eq!(
                app.get_token_endpoint_auth_method(),
                case["auth_method"].as_str().unwrap(),
                "auth_method for {name}"
            );
            assert_eq!(
                app.is_public_client(),
                case["is_public"].as_bool().unwrap(),
                "is_public for {name}"
            );
        }
    }

    #[test]
    fn validate_for_grant_type_matches_go() {
        for case in oracle()["validate_grant"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut app = valid_app();
            if name.starts_with("public") {
                app.client_secret = String::new();
            }

            let (grant, secret, verifier) = match name {
                "public_auth_code_with_pkce" => (ACCESS_TOKEN_GRANT_TYPE, "", "verifier"),
                "public_auth_code_without_pkce" => (ACCESS_TOKEN_GRANT_TYPE, "", ""),
                "public_with_secret" => (ACCESS_TOKEN_GRANT_TYPE, "some-secret", "verifier"),
                "public_refresh_token" => (REFRESH_TOKEN_GRANT_TYPE, "", "verifier"),
                "public_unknown_grant" => ("something_else", "", ""),
                "confidential_correct_secret" => (ACCESS_TOKEN_GRANT_TYPE, "a-client-secret", ""),
                "confidential_wrong_secret" => (ACCESS_TOKEN_GRANT_TYPE, "wrong", ""),
                "confidential_empty_secret" => (ACCESS_TOKEN_GRANT_TYPE, "", ""),
                "confidential_prefix_secret" => (ACCESS_TOKEN_GRANT_TYPE, "a-client-secre", ""),
                "confidential_refresh_token_ok" => {
                    (REFRESH_TOKEN_GRANT_TYPE, "a-client-secret", "")
                }
                other => panic!("unmapped: {other}"),
            };

            let got = app.validate_for_grant_type(grant, secret, verifier);
            if case["ok"].as_bool().unwrap() {
                assert!(got.is_ok(), "{name}: expected ok, got {got:?}");
            } else {
                let err = got.expect_err(&format!("{name}: expected an error"));
                assert_eq!(err.id, case["id"].as_str().unwrap(), "{name}: id");
                assert_eq!(err.where_, case["where"].as_str().unwrap(), "{name}: where");
                assert_eq!(
                    err.status_code,
                    case["status"].as_i64().unwrap() as i32,
                    "{name}: status — the credentials branch is the only 401"
                );
            }
        }
    }

    /// Includes the trailing-colon key, asserted explicitly.
    #[test]
    fn auditable_matches_go_including_the_typo_key() {
        let oracle = oracle();
        let expected: serde_json::Value =
            serde_json::from_str(oracle["auditable"]["confidential"].as_str().unwrap()).unwrap();
        assert_eq!(serde_json::Value::Object(valid_app().auditable()), expected);

        assert!(
            expected.get("callback_urls:").is_some(),
            "Go's key has a trailing colon; if upstream fixes it, this test says so"
        );
        assert!(expected.get("callback_urls").is_none());

        // The point of an Auditable implementation: the secret is not in it.
        assert!(expected.get("client_secret").is_none());

        let mut public = valid_app();
        public.client_secret = String::new();
        let expected_public: serde_json::Value =
            serde_json::from_str(oracle["auditable"]["public"].as_str().unwrap()).unwrap();
        assert_eq!(
            serde_json::Value::Object(public.auditable()),
            expected_public
        );
    }

    #[test]
    fn pre_save_and_pre_update_match_gos_invariants() {
        let oracle = oracle();
        let cases = oracle["pre_save"].as_array().unwrap();
        let case = |name: &str| {
            cases
                .iter()
                .find(|c| c["name"] == name)
                .unwrap_or_else(|| panic!("missing {name}"))
        };

        let with_id = case("pre_save_with_id");
        let mut app = valid_app();
        let before = app.clone();
        app.pre_save();
        assert_eq!(
            app.id == before.id,
            with_id["id_unchanged"].as_bool().unwrap()
        );
        assert_eq!(app.create_at, app.update_at);
        assert_ne!(app.create_at, 0);
        assert_eq!(app.client_secret, before.client_secret);

        let without_id = case("pre_save_without_id");
        let mut fresh = valid_app();
        fresh.id = String::new();
        fresh.client_secret = String::new();
        fresh.pre_save();
        assert!(!fresh.id.is_empty());
        assert_eq!(
            fresh.id.len(),
            without_id["id_length"].as_u64().unwrap() as usize
        );
        // Go's comment is explicit: PreSave no longer generates client secrets.
        assert!(
            fresh.client_secret.is_empty(),
            "PreSave must not mint a secret — an app saved without one is a public client"
        );

        let mut updated = valid_app();
        let create_at_before = updated.create_at;
        updated.pre_update();
        assert_eq!(updated.create_at, create_at_before);
        assert_ne!(updated.update_at, 0);
    }
}
