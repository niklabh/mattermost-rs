//! Port of `model/authorize.go`.
//!
//! The OAuth **authorization code** surface, including PKCE (RFC 7636) and resource indicators
//! (RFC 8707). Every branch here decides whether a code redemption proceeds, so a rule translated
//! the wrong way is an authentication bypass rather than a wire-format wobble. Nothing below is
//! written from a reading of the Go — see `fixtures/behaviour_authorize.json`.
//!
//! # Four things a reading gets wrong
//!
//! **1. `AuthorizeRequest::is_valid` reports the wrong `Where`.** All five of its own branches say
//! `AuthData.IsValid` — a copy-paste from the function above it — while the two it delegates say
//! `AuthorizeRequest.…`. The `Where` is therefore inconsistent *within one function*. Reproduced;
//! see [`AuthorizeRequest::is_valid`].
//!
//! **2. `IsExpired` overflows.** `ad.CreateAt + int64(ad.ExpiresIn*1000)` multiplies in **int32**
//! and widens afterwards, so any `expires_in` above 2,147,483 seconds wraps negative and the code
//! reads as long expired. `i32::MAX` gives `-1000`: a code claiming to last 68 years expires
//! immediately. Reproduced with `wrapping_mul` — see [`AuthData::is_expired`].
//!
//! **3. `VerifyPKCE` returns `true` when no PKCE was stored.** Deliberate backward compatibility,
//! and the most dangerous line in the file to get wrong in either direction. What stops it
//! mattering for a public client is [`AuthData::validate_pkce_for_client_type`], not this.
//!
//! **4. A trailing `#` is not a fragment.** `url.Parse("https://x/#")` yields an empty `Fragment`,
//! so RFC 8707's no-fragment rule passes. `https://x/#f` fails.
//!
//! # Two caps that read like characters and count bytes
//!
//! `Code` (128), `RedirectUri` (256), `State` (1024), `Scope` (128) and `Resource` (512) are all
//! `len()`, i.e. **bytes**. So 64 two-byte runes exactly fill the code cap and 65 overflow it. The
//! PKCE lengths are bytes too, but the charsets are ASCII-only so it cannot matter there.

use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::go_url;
use crate::utils::{self, AppResult, get_millis, is_valid_http_url, is_valid_id};

// ---------------------------------------------------------------------------
// Constants (authorize.go:19-29)
// ---------------------------------------------------------------------------

/// Port of `model.AuthCodeExpireTime` (authorize.go:20) — `60 * 10`.
///
/// **Seconds**, not milliseconds. [`AuthData::is_expired`] multiplies by 1000, so reading this as
/// milliseconds would shorten every authorization code to six tenths of a second.
pub const AUTH_CODE_EXPIRE_TIME: i32 = 60 * 10;

pub const AUTH_CODE_RESPONSE_TYPE: &str = "code";
pub const IMPLICIT_RESPONSE_TYPE: &str = "token";
pub const DEFAULT_SCOPE: &str = "user";
pub const PKCE_CODE_CHALLENGE_METHOD_S256: &str = "S256";
pub const PKCE_CODE_CHALLENGE_MIN_LENGTH: usize = 43;
pub const PKCE_CODE_CHALLENGE_MAX_LENGTH: usize = 128;
pub const PKCE_CODE_VERIFIER_MIN_LENGTH: usize = 43;
pub const PKCE_CODE_VERIFIER_MAX_LENGTH: usize = 128;

/// The caps `IsValid` applies, none of which has a named constant in Go.
const AUTH_CODE_MAX_LENGTH: usize = 128;
const REDIRECT_URI_MAX_LENGTH: usize = 256;
const STATE_MAX_LENGTH: usize = 1024;
const SCOPE_MAX_LENGTH: usize = 128;
const RESOURCE_MAX_LENGTH: usize = 512;

/// Port of `codeChallengeRegex` (authorize.go:15) — base64url, unpadded.
///
/// Go's `$` is end-of-**text** by default (RE2 behaves like `\z`, not Perl's `\Z`), and the
/// `regex` crate agrees, so a trailing newline is rejected by both. The class is ASCII in both.
static CODE_CHALLENGE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("^[A-Za-z0-9_-]+$").unwrap_or_else(|e| unreachable!("literal pattern: {e}"))
});

/// Port of `codeVerifierRegex` (authorize.go:16) — RFC 3986 unreserved characters.
static CODE_VERIFIER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[A-Za-z0-9\-._~]+$").unwrap_or_else(|e| unreachable!("literal pattern: {e}"))
});

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Port of `model.AuthData` (authorize.go:31) — a stored authorization code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthData {
    #[serde(rename = "client_id")]
    pub client_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "code")]
    pub code: String,

    /// **`int32` in Go**, and that is load-bearing rather than incidental — see
    /// [`AuthData::is_expired`], where the multiply overflows in this width.
    #[serde(rename = "expires_in")]
    pub expires_in: i32,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "redirect_uri")]
    pub redirect_uri: String,

    #[serde(rename = "state")]
    pub state: String,

    #[serde(rename = "scope")]
    pub scope: String,

    /// Go: non-pointer `string` with `omitempty`, so this is a `String` with a skip predicate and
    /// **not** an `Option` — the wire has two states, not three.
    #[serde(
        rename = "code_challenge",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub code_challenge: String,

    #[serde(
        rename = "code_challenge_method",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub code_challenge_method: String,

    #[serde(rename = "resource", default, skip_serializing_if = "String::is_empty")]
    pub resource: String,
}

/// Port of `model.AuthorizeRequest` (authorize.go:45).
///
/// Note `redirect_uri`'s Go field is spelled `RedirectURI` here and `RedirectUri` on [`AuthData`];
/// the JSON tag is the same on both, so only the Go call sites see the difference.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorizeRequest {
    #[serde(rename = "response_type")]
    pub response_type: String,

    #[serde(rename = "client_id")]
    pub client_id: String,

    #[serde(rename = "redirect_uri")]
    pub redirect_uri: String,

    #[serde(rename = "scope")]
    pub scope: String,

    #[serde(rename = "state")]
    pub state: String,

    #[serde(
        rename = "code_challenge",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub code_challenge: String,

    #[serde(
        rename = "code_challenge_method",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub code_challenge_method: String,

    #[serde(rename = "resource", default, skip_serializing_if = "String::is_empty")]
    pub resource: String,
}

/// The `AppError` shape every branch in this file produces: no params, always a 400.
fn authorize_error(where_: &str, id: &str, client_id: Option<&str>) -> Box<utils::AppError> {
    Box::new(utils::AppError::new(
        where_,
        id,
        None,
        client_id.map_or_else(String::new, |c| format!("client_id={c}")),
        400,
    ))
}

impl AuthData {
    /// Port of `(*AuthData).IsValid` (authorize.go:58).
    ///
    /// Nine checks in order, then the two conditional blocks. Three results the source does not
    /// advertise, all measured:
    ///
    /// - **A negative `expires_in` validates.** The guard is `== 0`, not `<= 0`, so `-1` passes
    ///   here and then makes [`Self::is_expired`] true forever.
    /// - **The caps count bytes.** 64 two-byte runes exactly fill the 128-byte code cap.
    /// - **The first two branches carry no detail at all**, while every later one carries
    ///   `client_id=` — so an invalid client id produces an error that does not say which client.
    pub fn is_valid(&self) -> AppResult {
        const W: &str = "AuthData.IsValid";

        if !is_valid_id(&self.client_id) {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.client_id.app_error",
                None,
            ));
        }

        if !is_valid_id(&self.user_id) {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.user_id.app_error",
                None,
            ));
        }

        if self.code.is_empty() || self.code.len() > AUTH_CODE_MAX_LENGTH {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.auth_code.app_error",
                Some(&self.client_id),
            ));
        }

        // `== 0`, not `<= 0` — a negative expiry is accepted here.
        if self.expires_in == 0 {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.expires.app_error",
                None,
            ));
        }

        if self.create_at <= 0 {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.create_at.app_error",
                Some(&self.client_id),
            ));
        }

        if self.redirect_uri.len() > REDIRECT_URI_MAX_LENGTH
            || !is_valid_http_url(&self.redirect_uri)
        {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.redirect_uri.app_error",
                Some(&self.client_id),
            ));
        }

        if self.state.len() > STATE_MAX_LENGTH {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.state.app_error",
                Some(&self.client_id),
            ));
        }

        if self.scope.len() > SCOPE_MAX_LENGTH {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.scope.app_error",
                Some(&self.client_id),
            ));
        }

        // Either PKCE field present pulls in both.
        if !self.code_challenge.is_empty() || !self.code_challenge_method.is_empty() {
            self.validate_pkce()?;
        }

        if !self.resource.is_empty() {
            validate_resource_parameter(&self.resource, &self.client_id, W)?;
        }

        Ok(())
    }

    /// Port of `(*AuthData).PreSave` (authorize.go:148).
    ///
    /// Three defaults, each guarded by `== 0` or `== ""` — so a **negative** `expires_in` is left
    /// alone rather than replaced.
    pub fn pre_save(&mut self) {
        if self.expires_in == 0 {
            self.expires_in = AUTH_CODE_EXPIRE_TIME;
        }

        if self.create_at == 0 {
            self.create_at = get_millis();
        }

        if self.scope.is_empty() {
            self.scope = DEFAULT_SCOPE.to_owned();
        }
    }

    /// Port of `(*AuthData).IsExpired` (authorize.go:162).
    ///
    /// # This overflows, and the overflow is reproduced
    ///
    /// ```text
    /// return GetMillis() > ad.CreateAt+int64(ad.ExpiresIn*1000)
    /// ```
    ///
    /// `ExpiresIn` is an `int32`, so `ExpiresIn*1000` is evaluated **at int32 width** and widened
    /// only afterwards. Go does not panic on non-constant integer overflow, so it wraps silently:
    ///
    /// | `expires_in` | Go's product | what the expression reads like |
    /// |---|---|---|
    /// | 600 | 600,000 | 600,000 |
    /// | 2,147,484 | −2,147,483,296 | 2,147,484,000 |
    /// | 2,147,483,647 | −1,000 | 2,147,483,647,000 |
    ///
    /// So a code with the largest expressible expiry is **already expired**. Written with
    /// `wrapping_mul` rather than `i64::from(self.expires_in) * 1000`, which would be the
    /// intuitive translation and would disagree with the Go server on the same database row.
    pub fn is_expired(&self) -> bool {
        get_millis() > self.expiry_threshold_millis()
    }

    /// The instant [`Self::is_expired`] compares the clock against.
    ///
    /// Split out because it is the deterministic half — the oracle can pin this where it cannot
    /// pin a clock-dependent bool.
    pub fn expiry_threshold_millis(&self) -> i64 {
        self.create_at + i64::from(self.expires_in.wrapping_mul(1000))
    }

    /// Port of `(*AuthData).validatePKCE` (authorize.go:195).
    fn validate_pkce(&self) -> AppResult {
        validate_pkce_parameters(
            &self.code_challenge,
            &self.code_challenge_method,
            &self.client_id,
            "AuthData.validatePKCE",
        )
    }

    /// Port of `(*AuthData).VerifyPKCE` (authorize.go:205).
    ///
    /// # `true` when no PKCE was stored
    ///
    /// Both fields empty means the flow never used PKCE, and Go returns `true` — accepting any
    /// verifier, including an empty one. That is deliberate backward compatibility and it is why
    /// [`Self::validate_pkce_for_client_type`] exists: for a public client, *that* function
    /// refuses before this one is ever consulted.
    ///
    /// Exactly one field empty is an impossible stored state and returns `false`.
    ///
    /// The challenge is compared in constant time. Go uses `==`; the values are derived from the
    /// client's own verifier and are not secrets, so this is not a divergence anything can
    /// observe — see [D-113].
    pub fn verify_pkce(&self, code_verifier: &str) -> bool {
        // No PKCE was used.
        if self.code_challenge.is_empty() && self.code_challenge_method.is_empty() {
            return true;
        }

        // Only one empty — an invalid stored state.
        if self.code_challenge.is_empty() || self.code_challenge_method.is_empty() {
            return false;
        }

        if code_verifier.len() < PKCE_CODE_VERIFIER_MIN_LENGTH
            || code_verifier.len() > PKCE_CODE_VERIFIER_MAX_LENGTH
        {
            return false;
        }

        if !CODE_VERIFIER_REGEX.is_match(code_verifier) {
            return false;
        }

        if self.code_challenge_method != PKCE_CODE_CHALLENGE_METHOD_S256 {
            return false;
        }

        // BASE64URL-ENCODE(SHA256(ASCII(code_verifier))), unpadded.
        let digest = Sha256::digest(code_verifier.as_bytes());
        let calculated = URL_SAFE_NO_PAD.encode(digest);

        use subtle::ConstantTimeEq as _;
        bool::from(calculated.as_bytes().ct_eq(self.code_challenge.as_bytes()))
    }

    /// Port of `(*AuthData).ValidatePKCEForClientType` (authorize.go:239).
    ///
    /// The guard that makes [`Self::verify_pkce`]'s permissive default safe.
    ///
    /// | client | stored challenge | verifier supplied | result |
    /// |---|---|---|---|
    /// | public | absent | — | `public_client_required` |
    /// | public | present | absent | `verifier_required` |
    /// | public | present | present | `verify_pkce` decides |
    /// | confidential | present | absent | `verifier_required` |
    /// | confidential | present | present | `verify_pkce` decides |
    /// | confidential | absent | present | `not_used_in_auth` |
    /// | confidential | absent | absent | allowed |
    ///
    /// Note the public branch tests **`code_challenge` only**, never the method. A stored record
    /// with a challenge and no method therefore reaches [`Self::verify_pkce`], which rejects it as
    /// an invalid state — so the failure is `verification_failed` rather than a missing-method
    /// error. Measured, not inferred.
    pub fn validate_pkce_for_client_type(
        &self,
        is_public_client: bool,
        code_verifier: &str,
    ) -> AppResult {
        const W: &str = "AuthData.ValidatePKCEForClientType";

        if is_public_client {
            // RFC 7636: public clients MUST use PKCE.
            if self.code_challenge.is_empty() {
                return Err(authorize_error(
                    W,
                    "model.authorize.validate_pkce.public_client_required.app_error",
                    Some(&self.client_id),
                ));
            }
            if code_verifier.is_empty() {
                return Err(authorize_error(
                    W,
                    "model.authorize.validate_pkce.verifier_required.app_error",
                    Some(&self.client_id),
                ));
            }
            if !self.verify_pkce(code_verifier) {
                return Err(authorize_error(
                    W,
                    "model.authorize.validate_pkce.verification_failed.app_error",
                    Some(&self.client_id),
                ));
            }
        } else if !self.code_challenge.is_empty() {
            // Confidential client that started the flow with PKCE must finish it with PKCE.
            if code_verifier.is_empty() {
                return Err(authorize_error(
                    W,
                    "model.authorize.validate_pkce.verifier_required.app_error",
                    Some(&self.client_id),
                ));
            }
            if !self.verify_pkce(code_verifier) {
                return Err(authorize_error(
                    W,
                    "model.authorize.validate_pkce.verification_failed.app_error",
                    Some(&self.client_id),
                ));
            }
        } else if !code_verifier.is_empty() {
            // A verifier for a flow that never used PKCE.
            return Err(authorize_error(
                W,
                "model.authorize.validate_pkce.not_used_in_auth.app_error",
                Some(&self.client_id),
            ));
        }

        Ok(())
    }
}

impl AuthorizeRequest {
    /// Port of `(*AuthorizeRequest).IsValid` (authorize.go:110).
    ///
    /// # Every branch it owns reports `AuthData.IsValid`
    ///
    /// A copy-paste from the function above it, and reproduced rather than corrected: the `Where`
    /// reaches the client inside the error envelope, so "fixing" it would make the two servers
    /// disagree about the body of the same 400. Its two *delegated* branches say
    /// `AuthorizeRequest.validatePKCE` and `AuthorizeRequest.IsValid`, so the value is
    /// inconsistent within this one function — which is what makes it recognisable as the bug it
    /// is. Pinned by `where_is_copy_pasted` in the oracle; if upstream fixes it, that test fails.
    ///
    /// # `response_type` is checked for emptiness only
    ///
    /// It is never compared against [`AUTH_CODE_RESPONSE_TYPE`] or [`IMPLICIT_RESPONSE_TYPE`], so
    /// `"banana"` validates. The narrowing happens at the handler, which is unported.
    ///
    /// # And `client_id` is checked before `response_type`
    ///
    /// The opposite of the struct's field order, so a request broken both ways reports the client
    /// id. Driven explicitly by the corpus.
    pub fn is_valid(&self) -> AppResult {
        // Upstream's, not a typo of ours. See the doc comment.
        const W: &str = "AuthData.IsValid";

        if !is_valid_id(&self.client_id) {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.client_id.app_error",
                None,
            ));
        }

        if self.response_type.is_empty() {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.response_type.app_error",
                None,
            ));
        }

        // The empty check is redundant — `is_valid_http_url("")` is already false — but it is
        // Go's, and removing it would be a behaviour-preserving edit to a security path.
        if self.redirect_uri.is_empty()
            || self.redirect_uri.len() > REDIRECT_URI_MAX_LENGTH
            || !is_valid_http_url(&self.redirect_uri)
        {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.redirect_uri.app_error",
                Some(&self.client_id),
            ));
        }

        if self.state.len() > STATE_MAX_LENGTH {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.state.app_error",
                Some(&self.client_id),
            ));
        }

        if self.scope.len() > SCOPE_MAX_LENGTH {
            return Err(authorize_error(
                W,
                "model.authorize.is_valid.scope.app_error",
                Some(&self.client_id),
            ));
        }

        if !self.code_challenge.is_empty() || !self.code_challenge_method.is_empty() {
            self.validate_pkce()?;
        }

        if !self.resource.is_empty() {
            validate_resource_parameter(
                &self.resource,
                &self.client_id,
                "AuthorizeRequest.IsValid",
            )?;
        }

        Ok(())
    }

    /// Port of `(*AuthorizeRequest).validatePKCE` (authorize.go:200).
    fn validate_pkce(&self) -> AppResult {
        validate_pkce_parameters(
            &self.code_challenge,
            &self.code_challenge_method,
            &self.client_id,
            "AuthorizeRequest.validatePKCE",
        )
    }
}

/// Port of `validatePKCEParameters` (authorize.go:167) — unexported in Go, shared by both types.
///
/// The order matters and is measured: presence of the challenge, then presence of the method,
/// then the method being `S256`, then the challenge's **length**, then its **format**. So a
/// too-short challenge with a `plain` method reports the unsupported method, not the length.
///
/// `S256` is compared case-sensitively — `s256` is rejected.
fn validate_pkce_parameters(
    code_challenge: &str,
    code_challenge_method: &str,
    client_id: &str,
    caller: &str,
) -> AppResult {
    if code_challenge.is_empty() {
        return Err(authorize_error(
            caller,
            "model.authorize.is_valid.code_challenge.app_error",
            Some(client_id),
        ));
    }

    if code_challenge_method.is_empty() {
        return Err(authorize_error(
            caller,
            "model.authorize.is_valid.code_challenge_method.app_error",
            Some(client_id),
        ));
    }

    // Only S256 is supported, for security — `plain` would make the challenge the verifier.
    if code_challenge_method != PKCE_CODE_CHALLENGE_METHOD_S256 {
        return Err(Box::new(utils::AppError::new(
            caller,
            "model.authorize.is_valid.code_challenge_method.unsupported.app_error",
            None,
            format!("client_id={client_id}, method={code_challenge_method}"),
            400,
        )));
    }

    if code_challenge.len() < PKCE_CODE_CHALLENGE_MIN_LENGTH
        || code_challenge.len() > PKCE_CODE_CHALLENGE_MAX_LENGTH
    {
        return Err(authorize_error(
            caller,
            "model.authorize.is_valid.code_challenge.length.app_error",
            Some(client_id),
        ));
    }

    if !CODE_CHALLENGE_REGEX.is_match(code_challenge) {
        return Err(authorize_error(
            caller,
            "model.authorize.is_valid.code_challenge.format.app_error",
            Some(client_id),
        ));
    }

    Ok(())
}

/// Port of `model.ValidateResourceParameter` (authorize.go:273) — RFC 8707.
///
/// Exported in Go and called from `access.go` as well as from both `IsValid`s here.
///
/// # It uses `url.Parse`, not `ParseRequestURI`
///
/// So it is lenient: a space, a multi-byte character and a bare `?` all pass. What *does* fail is
/// a control character, a malformed percent escape and a non-numeric port. `go_url::go_parse` is
/// the port of the right one — the `url` crate is WHATWG and would disagree in both directions.
///
/// # "Absolute" means "has a scheme", not "is an http URL"
///
/// `urn:example:resource` and `mailto:someone@example.com` both validate. Only `//host/path`,
/// `/path` and a bare hostname are rejected.
///
/// # A trailing `#` is not a fragment
///
/// `https://x/#` parses with an **empty** `Fragment`, so the no-fragment rule passes. `https://x/#f`
/// does not. Measured; the two differ by one byte of input.
pub fn validate_resource_parameter(resource: &str, client_id: &str, caller: &str) -> AppResult {
    // An absent resource is allowed. Both callers already guard on this, so it is reachable only
    // through the exported entry point.
    if resource.is_empty() {
        return Ok(());
    }

    // The cap exists to fit the database column, and is bytes.
    if resource.len() > RESOURCE_MAX_LENGTH {
        return Err(authorize_error(
            caller,
            "model.authorize.is_valid.resource.length.app_error",
            Some(client_id),
        ));
    }

    let Ok(parsed) = go_url::go_parse(resource) else {
        return Err(authorize_error(
            caller,
            "model.authorize.is_valid.resource.invalid_uri.app_error",
            Some(client_id),
        ));
    };

    // Go's `URL.IsAbs()` is `u.Scheme != ""`.
    if parsed.scheme.is_empty() {
        return Err(authorize_error(
            caller,
            "model.authorize.is_valid.resource.not_absolute.app_error",
            Some(client_id),
        ));
    }

    if !parsed.fragment.is_empty() {
        return Err(authorize_error(
            caller,
            "model.authorize.is_valid.resource.has_fragment.app_error",
            Some(client_id),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_auth_data() -> AuthData {
        AuthData {
            client_id: "abcdefghijklmnopqrstuvwxyz".into(),
            user_id: "zyxwvutsrqponmlkjihgfedcba".into(),
            code: "the-authorization-code".into(),
            expires_in: 600,
            create_at: 1_700_000_000_000,
            redirect_uri: "https://example.com/callback".into(),
            state: "opaque-state".into(),
            scope: "user".into(),
            ..Default::default()
        }
    }

    #[test]
    fn pre_save_defaults_only_zero_values() {
        let mut ad = AuthData::default();
        ad.pre_save();
        assert_eq!(ad.expires_in, AUTH_CODE_EXPIRE_TIME);
        assert_eq!(ad.scope, DEFAULT_SCOPE);
        assert!(ad.create_at > 0);

        // A negative expiry is left alone — the guard is `== 0`.
        let mut negative = AuthData {
            expires_in: -5,
            create_at: 1,
            scope: "custom".into(),
            ..Default::default()
        };
        negative.pre_save();
        assert_eq!(negative.expires_in, -5);
        assert_eq!(negative.create_at, 1);
        assert_eq!(negative.scope, "custom");
    }

    /// The int32 overflow, stated on its own.
    #[test]
    fn the_expiry_multiply_wraps_at_int32() {
        let mut ad = valid_auth_data();

        ad.expires_in = 600;
        assert_eq!(ad.expiry_threshold_millis(), ad.create_at + 600_000);

        // The largest expressible expiry produces a threshold BEFORE create_at.
        ad.expires_in = i32::MAX;
        assert_eq!(ad.expiry_threshold_millis(), ad.create_at - 1_000);
        assert!(ad.is_expired(), "a 68-year code is already expired");

        // And the intuitive translation would disagree.
        assert_ne!(
            ad.expiry_threshold_millis(),
            ad.create_at + i64::from(i32::MAX) * 1_000
        );
    }

    #[test]
    fn a_fresh_code_is_not_expired() {
        let ad = AuthData {
            create_at: get_millis(),
            expires_in: AUTH_CODE_EXPIRE_TIME,
            ..valid_auth_data()
        };
        assert!(!ad.is_expired());
    }

    /// The permissive default, and the guard that contains it.
    #[test]
    fn no_stored_pkce_accepts_any_verifier_but_not_from_a_public_client() {
        let ad = valid_auth_data();
        assert_eq!(ad.code_challenge, "");
        assert!(ad.verify_pkce(""), "backward compatibility");
        assert!(ad.verify_pkce("anything at all"));

        // A public client is refused before verify_pkce is consulted.
        let err = ad
            .validate_pkce_for_client_type(true, "anything at all")
            .unwrap_err();
        assert_eq!(
            err.id,
            "model.authorize.validate_pkce.public_client_required.app_error"
        );

        // A confidential client with no PKCE and no verifier is fine.
        assert!(ad.validate_pkce_for_client_type(false, "").is_ok());
        // But supplying a verifier it never committed to is not.
        let err = ad.validate_pkce_for_client_type(false, "x").unwrap_err();
        assert_eq!(
            err.id,
            "model.authorize.validate_pkce.not_used_in_auth.app_error"
        );
    }

    #[test]
    fn the_s256_challenge_is_unpadded_base64url() {
        let verifier = "abcdefghijklmnopqrstuvwxyz0123456789-._~ABC";
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);

        assert_eq!(challenge.len(), 43, "32 bytes unpadded");
        assert!(!challenge.contains('='), "no padding");

        let ad = AuthData {
            code_challenge: challenge,
            code_challenge_method: PKCE_CODE_CHALLENGE_METHOD_S256.into(),
            ..valid_auth_data()
        };
        assert!(ad.verify_pkce(verifier));
        assert!(!ad.verify_pkce(&format!("b{}", &verifier[1..])));
    }

    #[test]
    fn a_trailing_hash_is_not_a_fragment() {
        assert!(validate_resource_parameter("https://x/#", "cid", "probe").is_ok());
        let err = validate_resource_parameter("https://x/#f", "cid", "probe").unwrap_err();
        assert_eq!(
            err.id,
            "model.authorize.is_valid.resource.has_fragment.app_error"
        );
    }

    #[test]
    fn the_caps_count_bytes() {
        let mut ad = valid_auth_data();
        ad.code = "é".repeat(64); // 128 bytes, 64 runes
        assert_eq!(ad.code.len(), 128);
        assert!(ad.is_valid().is_ok());

        ad.code = "é".repeat(65);
        assert!(ad.is_valid().is_err());
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;
    use std::sync::OnceLock;

    fn oracle() -> &'static Value {
        static ORACLE: OnceLock<Value> = OnceLock::new();
        ORACLE.get_or_init(|| {
            let raw = include_str!("../../../fixtures/behaviour_authorize.json");
            serde_json::from_str(raw).expect("behaviour_authorize.json parses")
        })
    }

    const ID: &str = "abcdefghijklmnopqrstuvwxyz";
    const ID2: &str = "zyxwvutsrqponmlkjihgfedcba";
    const VERIFIER: &str = "abcdefghijklmnopqrstuvwxyz0123456789-._~ABC";

    fn s256(verifier: &str) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
    }

    fn base_auth_data() -> AuthData {
        AuthData {
            client_id: ID.into(),
            user_id: ID2.into(),
            code: "the-authorization-code".into(),
            expires_in: 600,
            create_at: 1_700_000_000_000,
            redirect_uri: "https://example.com/callback".into(),
            state: "opaque-state".into(),
            scope: "user".into(),
            ..Default::default()
        }
    }

    fn base_request() -> AuthorizeRequest {
        AuthorizeRequest {
            response_type: AUTH_CODE_RESPONSE_TYPE.into(),
            client_id: ID.into(),
            redirect_uri: "https://example.com/callback".into(),
            scope: "user".into(),
            state: "opaque-state".into(),
            ..Default::default()
        }
    }

    fn assert_error_matches(got: &AppResult, case: &Value, name: &str) {
        match got {
            Ok(()) => assert!(case["ok"].as_bool().unwrap(), "{name}: Go rejected this"),
            Err(err) => {
                assert!(!case["ok"].as_bool().unwrap(), "{name}: Go accepted this");
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
                    "{name}: detailed_error"
                );
            }
        }
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(c["AuthCodeExpireTime"], AUTH_CODE_EXPIRE_TIME);
        assert_eq!(c["AuthCodeResponseType"], AUTH_CODE_RESPONSE_TYPE);
        assert_eq!(c["ImplicitResponseType"], IMPLICIT_RESPONSE_TYPE);
        assert_eq!(c["DefaultScope"], DEFAULT_SCOPE);
        assert_eq!(
            c["PKCECodeChallengeMethodS256"],
            PKCE_CODE_CHALLENGE_METHOD_S256
        );
        assert_eq!(
            c["PKCECodeChallengeMinLength"],
            PKCE_CODE_CHALLENGE_MIN_LENGTH
        );
        assert_eq!(
            c["PKCECodeChallengeMaxLength"],
            PKCE_CODE_CHALLENGE_MAX_LENGTH
        );
        assert_eq!(
            c["PKCECodeVerifierMinLength"],
            PKCE_CODE_VERIFIER_MIN_LENGTH
        );
        assert_eq!(
            c["PKCECodeVerifierMaxLength"],
            PKCE_CODE_VERIFIER_MAX_LENGTH
        );
        assert_eq!(
            c["auth_code_expire_time_unit"], "seconds",
            "is_expired multiplies by 1000; reading this as ms would break every code"
        );
    }

    #[test]
    fn auth_data_is_valid_matches_go() {
        let max_challenge = "a".repeat(PKCE_CODE_CHALLENGE_MAX_LENGTH);
        let min_challenge = "a".repeat(PKCE_CODE_CHALLENGE_MIN_LENGTH);

        for case in oracle()["auth_data_is_valid"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut ad = base_auth_data();

            match name {
                "valid" | "pkce_both_empty_is_skipped" => {}
                "bad_client_id" => ad.client_id = "nope".into(),
                "empty_client_id" => ad.client_id = String::new(),
                "bad_user_id" => ad.user_id = "nope".into(),
                "empty_code" => ad.code = String::new(),
                "code_at_cap" => ad.code = "c".repeat(128),
                "code_over_cap" => ad.code = "c".repeat(129),
                "code_multibyte_at_cap" => ad.code = "é".repeat(64),
                "code_multibyte_over_cap" => ad.code = "é".repeat(65),
                "zero_expires_in" => ad.expires_in = 0,
                "negative_expires_in" => ad.expires_in = -1,
                "zero_create_at" => ad.create_at = 0,
                "negative_create_at" => ad.create_at = -1,
                "empty_redirect_uri" => ad.redirect_uri = String::new(),
                "non_http_redirect_uri" => ad.redirect_uri = "ftp://example.com/x".into(),
                "relative_redirect_uri" => ad.redirect_uri = "/callback".into(),
                "redirect_uri_at_cap" => {
                    ad.redirect_uri = format!("https://example.com/{}", "a".repeat(256 - 20));
                }
                "redirect_uri_over_cap" => {
                    ad.redirect_uri = format!("https://example.com/{}", "a".repeat(257 - 20));
                }
                "state_at_cap" => ad.state = "s".repeat(1024),
                "state_over_cap" => ad.state = "s".repeat(1025),
                "scope_at_cap" => ad.scope = "s".repeat(128),
                "scope_over_cap" => ad.scope = "s".repeat(129),
                "pkce_challenge_only" => ad.code_challenge = min_challenge.clone(),
                "pkce_method_only" => ad.code_challenge_method = "S256".into(),
                "pkce_valid" => {
                    ad.code_challenge = min_challenge.clone();
                    ad.code_challenge_method = "S256".into();
                }
                "pkce_plain_method" => {
                    ad.code_challenge = min_challenge.clone();
                    ad.code_challenge_method = "plain".into();
                }
                "pkce_lowercase_method" => {
                    ad.code_challenge = min_challenge.clone();
                    ad.code_challenge_method = "s256".into();
                }
                "pkce_challenge_under_min" => {
                    ad.code_challenge = "a".repeat(PKCE_CODE_CHALLENGE_MIN_LENGTH - 1);
                    ad.code_challenge_method = "S256".into();
                }
                "pkce_challenge_at_max" => {
                    ad.code_challenge = max_challenge.clone();
                    ad.code_challenge_method = "S256".into();
                }
                "pkce_challenge_over_max" => {
                    ad.code_challenge = "a".repeat(PKCE_CODE_CHALLENGE_MAX_LENGTH + 1);
                    ad.code_challenge_method = "S256".into();
                }
                "pkce_challenge_bad_charset" => {
                    ad.code_challenge = format!("{}+", "a".repeat(42));
                    ad.code_challenge_method = "S256".into();
                }
                "pkce_challenge_with_padding" => {
                    ad.code_challenge = format!("{}=", "a".repeat(42));
                    ad.code_challenge_method = "S256".into();
                }
                "resource_valid" => ad.resource = "https://api.example.com/v1".into(),
                "resource_relative" => ad.resource = "/v1/resource".into(),
                "resource_with_fragment" => ad.resource = "https://api.example.com/#frag".into(),
                "resource_at_cap" => {
                    ad.resource = format!("https://api.example.com/{}", "r".repeat(512 - 24));
                }
                "resource_over_cap" => {
                    ad.resource = format!("https://api.example.com/{}", "r".repeat(513 - 24));
                }
                "bad_client_id_and_bad_user_id" => {
                    ad.client_id = "nope".into();
                    ad.user_id = "nope".into();
                }
                "bad_code_and_bad_pkce" => {
                    ad.code = String::new();
                    ad.code_challenge = "+".into();
                    ad.code_challenge_method = "plain".into();
                }
                "bad_pkce_and_bad_resource" => {
                    ad.code_challenge_method = "plain".into();
                    ad.code_challenge = "a".repeat(43);
                    ad.resource = "/relative".into();
                }
                other => panic!("unmapped corpus case: {other}"),
            }

            assert_error_matches(&ad.is_valid(), case, name);
        }
    }

    #[test]
    fn authorize_request_is_valid_matches_go() {
        let min_challenge = "a".repeat(PKCE_CODE_CHALLENGE_MIN_LENGTH);

        for case in oracle()["authorize_request_is_valid"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut ar = base_request();

            match name {
                "valid" => {}
                "implicit_response_type" => ar.response_type = IMPLICIT_RESPONSE_TYPE.into(),
                "nonsense_response_type" => ar.response_type = "banana".into(),
                "empty_response_type" => ar.response_type = String::new(),
                "bad_client_id" => ar.client_id = "nope".into(),
                "empty_redirect_uri" => ar.redirect_uri = String::new(),
                "relative_redirect_uri" => ar.redirect_uri = "/callback".into(),
                "redirect_uri_over_cap" => {
                    ar.redirect_uri = format!("https://example.com/{}", "a".repeat(257 - 20));
                }
                "state_over_cap" => ar.state = "s".repeat(1025),
                "scope_over_cap" => ar.scope = "s".repeat(129),
                "pkce_challenge_only" => ar.code_challenge = min_challenge.clone(),
                "pkce_method_only" => ar.code_challenge_method = "S256".into(),
                "pkce_valid" => {
                    ar.code_challenge = min_challenge.clone();
                    ar.code_challenge_method = "S256".into();
                }
                "pkce_plain_method" => {
                    ar.code_challenge = min_challenge.clone();
                    ar.code_challenge_method = "plain".into();
                }
                "resource_relative" => ar.resource = "/v1/resource".into(),
                "resource_with_fragment" => ar.resource = "https://api.example.com/#f".into(),
                "bad_client_id_and_empty_response_type" => {
                    ar.client_id = "nope".into();
                    ar.response_type = String::new();
                }
                other => panic!("unmapped corpus case: {other}"),
            }

            assert_error_matches(&ar.is_valid(), case, name);
        }
    }

    /// The copy-pasted `Where`, asserted as the claim it is.
    ///
    /// If upstream ever repairs it, this test fails — which is the signal we want, the same
    /// treatment [D-016] and [D-019] get.
    #[test]
    fn authorize_request_reports_auth_datas_where() {
        let w = &oracle()["where_is_copy_pasted"];
        assert_eq!(w["own_branch_names_the_wrong_type"], true);
        assert_eq!(w["authorize_request_own_branch"], "AuthData.IsValid");
        assert_eq!(w["auth_data_own_branch"], "AuthData.IsValid");
        assert_eq!(
            w["authorize_request_pkce_branch"], "AuthorizeRequest.validatePKCE",
            "the delegated branches name the right type — which is what makes it a bug"
        );
        assert_eq!(
            w["authorize_request_resource_branch"],
            "AuthorizeRequest.IsValid"
        );
        assert_eq!(w["delegated_branches_name_the_right_one"], true);

        // And ours does the same.
        let ar = AuthorizeRequest {
            client_id: "nope".into(),
            ..base_request()
        };
        assert_eq!(ar.is_valid().unwrap_err().where_, "AuthData.IsValid");
    }

    #[test]
    fn pre_save_matches_go() {
        for case in oracle()["pre_save"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut ad = AuthData {
                expires_in: case["in_expires_in"].as_i64().unwrap() as i32,
                create_at: case["in_create_at"].as_i64().unwrap(),
                scope: case["in_scope"].as_str().unwrap().to_owned(),
                ..Default::default()
            };
            ad.pre_save();

            assert_eq!(
                i64::from(ad.expires_in),
                case["out_expires_in"].as_i64().unwrap(),
                "{name}: expires_in"
            );
            assert_eq!(
                ad.scope,
                case["out_scope"].as_str().unwrap(),
                "{name}: scope"
            );

            if case["create_at_uses_now"].as_bool().unwrap() {
                assert!(ad.create_at > 0, "{name}: create_at came from the clock");
            } else {
                assert_eq!(
                    ad.create_at,
                    case["out_create_at"].as_i64().unwrap(),
                    "{name}: create_at untouched"
                );
            }
        }
    }

    /// The int32 overflow, against Go's own arithmetic.
    #[test]
    fn the_expiry_threshold_matches_go() {
        let mut overflowing = 0;

        for case in oracle()["is_expired"].as_array().unwrap() {
            let expires_in = case["expires_in"].as_i64().unwrap() as i32;
            let create_at = case["create_at"].as_i64().unwrap();
            let ad = AuthData {
                expires_in,
                create_at,
                ..base_auth_data()
            };

            assert_eq!(
                ad.expiry_threshold_millis(),
                case["expiry_threshold"].as_i64().unwrap(),
                "expires_in={expires_in}: threshold"
            );

            if case["overflows"].as_bool().unwrap() {
                overflowing += 1;
                assert_ne!(
                    case["wrapped_product"], case["widened_product"],
                    "expires_in={expires_in}: the corpus says this overflows"
                );
                // The intuitive translation would have disagreed here.
                assert_ne!(
                    ad.expiry_threshold_millis(),
                    create_at + i64::from(expires_in) * 1000,
                    "expires_in={expires_in}: i64 widening before the multiply is the wrong port"
                );
            }

            if case["threshold_is_before_create_at"].as_bool().unwrap() {
                assert!(
                    ad.expiry_threshold_millis() < create_at,
                    "expires_in={expires_in}: already expired"
                );
            }
        }

        assert!(overflowing >= 3, "the corpus must exercise the overflow");
    }

    #[test]
    fn verify_pkce_matches_go() {
        for case in oracle()["verify_pkce"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let ad = AuthData {
                code_challenge: case["code_challenge"].as_str().unwrap().to_owned(),
                code_challenge_method: case["method"].as_str().unwrap().to_owned(),
                ..base_auth_data()
            };
            let verifier = case["code_verifier"].as_str().unwrap();

            assert_eq!(
                ad.verify_pkce(verifier),
                case["verified"].as_bool().unwrap(),
                "{name}"
            );
        }

        // And our S256 agrees with Go's, which the corpus above assumes.
        let challenge = oracle()["verify_pkce"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "correct_verifier")
            .unwrap()["code_challenge"]
            .as_str()
            .unwrap();
        assert_eq!(s256(VERIFIER), challenge);
    }

    #[test]
    fn validate_pkce_for_client_type_matches_go() {
        for case in oracle()["validate_pkce_for_client"].as_array().unwrap() {
            let name = format!(
                "{}/{}",
                case["name"].as_str().unwrap(),
                if case["is_public_client"].as_bool().unwrap() {
                    "public"
                } else {
                    "confidential"
                }
            );
            let ad = AuthData {
                code_challenge: case["code_challenge"].as_str().unwrap().to_owned(),
                code_challenge_method: case["method"].as_str().unwrap().to_owned(),
                ..base_auth_data()
            };

            let got = ad.validate_pkce_for_client_type(
                case["is_public_client"].as_bool().unwrap(),
                case["code_verifier"].as_str().unwrap(),
            );
            assert_error_matches(&got, case, &name);
        }
    }

    #[test]
    fn validate_resource_parameter_matches_go() {
        for case in oracle()["validate_resource_parameter"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let got = validate_resource_parameter(case["resource"].as_str().unwrap(), ID, "probe");
            assert_error_matches(&got, case, name);
        }
    }

    /// The two charsets, swept through the positions that apply them.
    ///
    /// Driven through `IsValid` and `VerifyPKCE` rather than against the regexes directly, because
    /// that measures the class **where it is used** — including the length and method checks that
    /// sit in front of it.
    #[test]
    fn the_pkce_charsets_match_go() {
        let challenge_probes = oracle()["code_challenge_charset"].as_array().unwrap();
        let verifier_probes = oracle()["code_verifier_charset"].as_array().unwrap();
        assert!(challenge_probes.len() >= 130 && verifier_probes.len() >= 130);

        let mut accepted_challenge = 0;
        let mut accepted_verifier = 0;

        for probe in challenge_probes {
            let cp = u32::try_from(probe["codepoint"].as_i64().unwrap()).unwrap();
            let ch = char::from_u32(cp).unwrap();
            let challenge = format!("{}{ch}{}", "a".repeat(20), "a".repeat(22));

            let ad = AuthData {
                code_challenge: challenge,
                code_challenge_method: "S256".into(),
                ..base_auth_data()
            };
            let got = ad.is_valid();
            assert_eq!(
                got.is_ok(),
                probe["ok"].as_bool().unwrap(),
                "challenge U+{cp:04X}"
            );
            match got {
                Ok(()) => accepted_challenge += 1,
                Err(e) => assert_eq!(e.id, probe["id"].as_str().unwrap(), "challenge U+{cp:04X}"),
            }
        }

        for probe in verifier_probes {
            let cp = u32::try_from(probe["codepoint"].as_i64().unwrap()).unwrap();
            let ch = char::from_u32(cp).unwrap();
            let verifier = format!("{}{ch}{}", "a".repeat(20), "a".repeat(22));

            let ad = AuthData {
                code_challenge: s256(&verifier),
                code_challenge_method: "S256".into(),
                ..base_auth_data()
            };
            let verified = ad.verify_pkce(&verifier);
            assert_eq!(
                verified,
                probe["verified"].as_bool().unwrap(),
                "verifier U+{cp:04X}"
            );
            if verified {
                accepted_verifier += 1;
            }
        }

        // The sweeps must not be degenerate in either direction.
        assert!(
            accepted_challenge > 30 && accepted_challenge < challenge_probes.len(),
            "challenge sweep accepted {accepted_challenge}"
        );
        assert!(
            accepted_verifier > 30 && accepted_verifier < verifier_probes.len(),
            "verifier sweep accepted {accepted_verifier}"
        );
        // The two classes are NOT the same: `_` is base64url-only, `.~` are unreserved-only.
        assert_ne!(
            accepted_challenge, accepted_verifier,
            "codeChallengeRegex and codeVerifierRegex differ"
        );
    }

    #[test]
    fn the_wire_format_matches_go() {
        for (fixture, is_auth_data) in [("auth_data", true), ("authorize_request", false)] {
            let raw = match fixture {
                "auth_data" => include_str!("../../../fixtures/auth_data.json"),
                _ => include_str!("../../../fixtures/authorize_request.json"),
            };
            let want: Value = serde_json::from_str(raw).expect("fixture parses");

            let round_tripped: Value = if is_auth_data {
                let decoded: AuthData = serde_json::from_str(raw).expect("decodes");
                serde_json::from_str(&utils::go_json_marshal(&decoded).expect("marshals"))
                    .expect("re-parses")
            } else {
                let decoded: AuthorizeRequest = serde_json::from_str(raw).expect("decodes");
                serde_json::from_str(&utils::go_json_marshal(&decoded).expect("marshals"))
                    .expect("re-parses")
            };

            assert_eq!(round_tripped, want, "{fixture}: round trip");
        }
    }

    /// `omitempty` on the three optional fields: absent, not `""`.
    #[test]
    fn the_optional_fields_are_omitted_when_empty() {
        let ad = base_auth_data();
        let json: Value = serde_json::from_str(&utils::go_json_marshal(&ad).unwrap()).unwrap();

        for key in ["code_challenge", "code_challenge_method", "resource"] {
            assert!(
                json.get(key).is_none(),
                "{key} should be omitted when empty"
            );
        }
        // While the eight non-omitempty keys survive their zero values.
        let zero: Value =
            serde_json::from_str(&utils::go_json_marshal(&AuthData::default()).unwrap()).unwrap();
        assert_eq!(zero.as_object().unwrap().len(), 8);
    }
}
