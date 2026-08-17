//! Port of `server/public/model/oauth_dcr.go` — OAuth 2.0 Dynamic Client Registration.
//!
//! Most of this file is **redirect-URI allowlist matching**, which is where open-redirect
//! vulnerabilities live: a matcher one case too permissive hands an attacker a token, one case
//! too strict breaks a working client. Neither failure is visible from reading the code, so every
//! branch here is pinned against Go by `fixtures/behaviour_oauth_dcr.json` — 44 glob probes, 25
//! pattern-validity probes and 11 allowlist probes.
//!
//! Four properties worth knowing before touching anything here:
//!
//! * **An empty allowlist permits everything** — see [`redirect_uri_matches_allowlist`].
//! * **The matcher compares bytes, not characters**, so `*` can match part of a multi-byte
//!   character.
//! * **`*` stops at `/` and `**` does not**, and matching is per URL component, so a host
//!   wildcard cannot satisfy a path requirement.
//! * **Pattern validation substitutes placeholders before parsing** — see
//!   [`is_valid_dcr_redirect_uri_pattern`].

use serde::{Deserialize, Serialize};

use crate::go_url;
use crate::utils::{AppError, AppResult, StringArray, is_valid_http_url};

/// Constants borrowed from `oauth_metadata.go`, which this port has not reached. Pinned by the
/// oracle so a drift upstream fails a test.
pub mod external {
    /// oauth_metadata.go:21
    pub const GRANT_TYPE_AUTHORIZATION_CODE: &str = "authorization_code";
    /// oauth_metadata.go:22
    pub const GRANT_TYPE_REFRESH_TOKEN: &str = "refresh_token";
    /// oauth_metadata.go:24
    pub const RESPONSE_TYPE_CODE: &str = "code";
}
use external::*;

/// oauth_dcr.go:31
pub const DCR_ERROR_INVALID_REDIRECT_URI: &str = "invalid_redirect_uri";
/// oauth_dcr.go:32
pub const DCR_ERROR_INVALID_CLIENT_METADATA: &str = "invalid_client_metadata";
/// oauth_dcr.go:33
pub const DCR_ERROR_UNSUPPORTED_OPERATION: &str = "unsupported_operation";

const CLIENT_NAME_MAX_BYTES: usize = 64;
const CLIENT_URI_MAX_BYTES: usize = 256;

/// Port of `model.ClientRegistrationRequest` (oauth_dcr.go:12).
///
/// Three of the four fields are `omitempty` **pointers**, so a pointer to `""` is not the same
/// document as a nil one — and, more importantly, not the same input: the validity checks below
/// are skipped for nil and applied for empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRegistrationRequest {
    #[serde(rename = "redirect_uris")]
    pub redirect_uris: Option<StringArray>,

    #[serde(
        rename = "token_endpoint_auth_method",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub token_endpoint_auth_method: Option<String>,

    #[serde(
        rename = "client_name",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_name: Option<String>,

    #[serde(
        rename = "client_uri",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_uri: Option<String>,
}

/// Port of `model.ClientRegistrationResponse` (oauth_dcr.go:19).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientRegistrationResponse {
    #[serde(rename = "client_id")]
    pub client_id: String,

    /// Present only for a confidential client — `OAuthApp::to_client_registration_response`
    /// leaves it `None` for a public one.
    #[serde(
        rename = "client_secret",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_secret: Option<String>,

    #[serde(rename = "redirect_uris")]
    pub redirect_uris: Option<StringArray>,

    #[serde(rename = "token_endpoint_auth_method")]
    pub token_endpoint_auth_method: String,

    #[serde(rename = "grant_types")]
    pub grant_types: Option<StringArray>,

    #[serde(rename = "response_types")]
    pub response_types: Option<StringArray>,

    #[serde(rename = "scope", default, skip_serializing_if = "String::is_empty")]
    pub scope: String,

    #[serde(
        rename = "client_name",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_name: Option<String>,

    #[serde(
        rename = "client_uri",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub client_uri: Option<String>,
}

/// Port of `model.DCRError` (oauth_dcr.go:36).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DcrError {
    #[serde(rename = "error")]
    pub error: String,
    #[serde(
        rename = "error_description",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub error_description: String,
}

/// Port of `NewDCRError` (oauth_dcr.go:79).
pub fn new_dcr_error(error_type: &str, description: &str) -> DcrError {
    DcrError {
        error: error_type.to_owned(),
        error_description: description.to_owned(),
    }
}

/// Port of `GetDefaultGrantTypes` (oauth_dcr.go:86).
pub fn get_default_grant_types() -> Vec<String> {
    vec![
        GRANT_TYPE_AUTHORIZATION_CODE.to_owned(),
        GRANT_TYPE_REFRESH_TOKEN.to_owned(),
    ]
}

/// Port of `GetDefaultResponseTypes` (oauth_dcr.go:90).
pub fn get_default_response_types() -> Vec<String> {
    vec![RESPONSE_TYPE_CODE.to_owned()]
}

impl ClientRegistrationRequest {
    fn error(&self, id: &str, detail: String) -> Box<AppError> {
        Box::new(AppError::new(
            "ClientRegistrationRequest.IsValid",
            id,
            None,
            detail,
            400,
        ))
    }

    /// Port of `(*ClientRegistrationRequest).IsValid` (oauth_dcr.go:48).
    ///
    /// # The client-URI checks run format-first
    ///
    /// Go validates the URI's *shape* before its *length*, so a value that is both malformed and
    /// over 256 bytes reports `client_uri_format`, not `client_uri_length`. Measured — the two
    /// have different error ids and a client may branch on them.
    ///
    /// A `None` field is skipped entirely; `Some("")` is checked and, for `client_name`, passes.
    pub fn is_valid(&self) -> AppResult {
        let redirect_uris = self.redirect_uris.as_deref().unwrap_or_default();
        if redirect_uris.is_empty() {
            return Err(self.error("model.dcr.is_valid.redirect_uris.app_error", String::new()));
        }

        for uri in redirect_uris {
            if !is_valid_http_url(uri) {
                return Err(self.error(
                    "model.dcr.is_valid.redirect_uri_format.app_error",
                    format!("uri={uri}"),
                ));
            }
        }

        if let Some(name) = &self.client_name
            && name.len() > CLIENT_NAME_MAX_BYTES
        {
            return Err(self.error("model.dcr.is_valid.client_name.app_error", String::new()));
        }

        if let Some(uri) = &self.client_uri {
            // Format before length — see the note above.
            if !is_valid_http_url(uri) {
                return Err(self.error(
                    "model.dcr.is_valid.client_uri_format.app_error",
                    format!("uri={uri}"),
                ));
            }
            if uri.len() > CLIENT_URI_MAX_BYTES {
                return Err(self.error(
                    "model.dcr.is_valid.client_uri_length.app_error",
                    String::new(),
                ));
            }
        }

        if let Some(method) = &self.token_endpoint_auth_method
            && method != crate::oauth::external::CLIENT_AUTH_METHOD_CLIENT_SECRET_POST
            && method != crate::oauth::external::CLIENT_AUTH_METHOD_NONE
        {
            return Err(self.error(
                "model.dcr.is_valid.unsupported_auth_method.app_error",
                format!("method={method}"),
            ));
        }

        Ok(())
    }
}

/// Port of `IsValidDCRRedirectURIPattern` (oauth_dcr.go:97).
///
/// # The placeholder substitution is the trick
///
/// A pattern is not a URL — `https://localhost:*` has a wildcard where the port belongs — so Go
/// cannot hand it to the URL parser directly. It replaces `**` and then `*` with the digit `1`
/// and validates *that*. The digit is chosen deliberately: a wildcarded port normalises to
/// `localhost:1`, which parses, where a letter would not.
///
/// The two-step replacement (`**` → placeholder → `1`, then `*` → placeholder → `1`) exists so a
/// `**` is not seen as two `*`s. `***` is rejected before any of it.
pub fn is_valid_dcr_redirect_uri_pattern(pattern: &str) -> bool {
    // Go's minimum-length checks are on the *byte* length and include the scheme, i.e. at least
    // one character after `https://`.
    if let Some(rest) = pattern.strip_prefix("https://") {
        if rest.is_empty() {
            return false;
        }
    } else if let Some(rest) = pattern.strip_prefix("http://") {
        if rest.is_empty() {
            return false;
        }
    } else {
        return false;
    }

    // `for _, r := range pattern` iterates runes, so this test is on code points.
    for ch in pattern.chars() {
        if (ch as u32) < 0x20 || ch as u32 == 0x7f {
            return false;
        }
    }

    if pattern.contains("***") {
        return false;
    }

    let normalized = pattern.replace("**", "mmdoublewildcard");
    let normalized = normalized.replace('*', "mmsinglewildcard");
    let normalized = normalized.replace("mmdoublewildcard", "1");
    let normalized = normalized.replace("mmsinglewildcard", "1");

    is_valid_http_url(&normalized)
}

/// Port of the unexported `dcrRedirectURIPattern` (oauth_dcr.go:41).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DcrRedirectUriPattern {
    scheme: String,
    host: String,
    path: String,
    raw_query: String,
}

/// Port of `parseDCRRedirectURIPattern` (oauth_dcr.go:166).
///
/// Splits the pattern by hand rather than parsing it, because a pattern is not a valid URL. The
/// host ends at whichever of `/` or `?` comes first; an empty host is a failure.
fn parse_dcr_redirect_uri_pattern(pattern: &str) -> Option<DcrRedirectUriPattern> {
    let (scheme, rest) = pattern.split_once("://")?;

    let mut host_end = rest.len();
    for separator in ['/', '?'] {
        if let Some(index) = rest.find(separator)
            && index < host_end
        {
            host_end = index;
        }
    }

    let host = &rest[..host_end];
    if host.is_empty() {
        return None;
    }

    let remainder = &rest[host_end..];
    let mut path = "";
    let mut raw_query = "";
    if remainder.starts_with('/') {
        // `strings.Cut` on the first `?`; with no `?` the whole remainder is the path.
        match remainder.split_once('?') {
            Some((p, q)) => {
                path = p;
                raw_query = q;
            }
            None => path = remainder,
        }
    } else if let Some(stripped) = remainder.strip_prefix('?') {
        raw_query = stripped;
    }

    Some(DcrRedirectUriPattern {
        scheme: scheme.to_owned(),
        host: host.to_owned(),
        path: path.to_owned(),
        raw_query: raw_query.to_owned(),
    })
}

/// Port of `redirectURIMatchesGlobRecur` (oauth_dcr.go:202).
///
/// # Bytes, not characters
///
/// Go indexes `uri[ui]` and `pattern[pi]`, which are bytes. So a multi-byte character occupies
/// several positions and `*` can match a fragment of one — measured: `caf*` matches `café`,
/// consuming both bytes of `é`. Taking `&[u8]` here rather than `&str` makes that structural
/// instead of accidental.
///
/// `*` matches zero or more bytes **except** `/`; `**` matches any bytes including `/`.
fn redirect_uri_matches_glob_recur(
    uri: &[u8],
    pattern: &[u8],
    mut ui: usize,
    mut pi: usize,
) -> bool {
    while pi < pattern.len() {
        if pattern[pi] == b'*' {
            if pi + 1 < pattern.len() && pattern[pi + 1] == b'*' {
                // `**` — any bytes, including `/`.
                pi += 2;
                if pi >= pattern.len() {
                    return true;
                }
                while ui <= uri.len() {
                    if redirect_uri_matches_glob_recur(uri, pattern, ui, pi) {
                        return true;
                    }
                    ui += 1;
                }
                return false;
            }

            // `*` — zero or more bytes, stopping at `/`.
            if redirect_uri_matches_glob_recur(uri, pattern, ui, pi + 1) {
                return true;
            }
            while ui < uri.len() && uri[ui] != b'/' {
                ui += 1;
                if redirect_uri_matches_glob_recur(uri, pattern, ui, pi + 1) {
                    return true;
                }
            }
            return false;
        }

        if ui >= uri.len() || uri[ui] != pattern[pi] {
            return false;
        }
        ui += 1;
        pi += 1;
    }

    ui == uri.len()
}

/// Port of `RedirectURIMatchesGlob` (oauth_dcr.go:135).
///
/// # Component-aware, which is the security property
///
/// The candidate is parsed and its scheme, host, path and query are matched **separately**, so a
/// wildcard in one component cannot satisfy a requirement in another. Measured:
/// `https://example.com/evil` does **not** match `https://*/cb`, even though a naive whole-string
/// glob would accept it.
///
/// The query rule is asymmetric and worth stating: a pattern with no query requires the candidate
/// to have none, and a pattern *with* a query rejects a candidate that has none.
pub fn redirect_uri_matches_glob(uri: &str, pattern: &str) -> bool {
    let Ok(candidate) = go_url::parse_request_uri(uri) else {
        return false;
    };
    if candidate.scheme.is_empty() || candidate.host.is_empty() {
        return false;
    }

    if !is_valid_dcr_redirect_uri_pattern(pattern) {
        return false;
    }

    let Some(parsed) = parse_dcr_redirect_uri_pattern(pattern) else {
        return false;
    };

    if candidate.scheme != parsed.scheme {
        return false;
    }
    if !redirect_uri_matches_glob_recur(&candidate.host, parsed.host.as_bytes(), 0, 0) {
        return false;
    }
    if !redirect_uri_matches_glob_recur(&candidate.escaped_path(), parsed.path.as_bytes(), 0, 0) {
        return false;
    }

    if parsed.raw_query.is_empty() {
        return candidate.raw_query.is_empty();
    }
    if candidate.raw_query.is_empty() {
        return false;
    }
    redirect_uri_matches_glob_recur(
        candidate.raw_query.as_bytes(),
        parsed.raw_query.as_bytes(),
        0,
        0,
    )
}

/// Port of `RedirectURIMatchesAllowlist` (oauth_dcr.go:238).
///
/// # An empty allowlist permits everything
///
/// Go's comment says it outright: *"If allowlist is empty, returns true (no restriction)."* That
/// is the permissive default, and it is load-bearing — a port that failed closed here would break
/// every deployment that has not configured one.
///
/// The **non**-empty cases are where the asymmetry bites. Each entry is trimmed and skipped if it
/// is then blank, so an allowlist of nothing but blanks is *not* empty and matches nothing —
/// denying everything. Measured, both directions:
///
/// | allowlist | result |
/// |---|---|
/// | `nil` / `[]` | **allowed** |
/// | `["", "   ", "\t"]` | **denied** |
/// | `["***"]` (invalid pattern) | denied |
/// | `["***", "https://ok"]` | allowed — one bad entry does not disable the list |
pub fn redirect_uri_matches_allowlist(uri: &str, allowlist: &[String]) -> bool {
    if allowlist.is_empty() {
        return true;
    }

    allowlist.iter().any(|pattern| {
        let trimmed = go_trim_space(pattern);
        !trimmed.is_empty() && redirect_uri_matches_glob(uri, trimmed)
    })
}

/// Go's `strings.TrimSpace`, which trims by `unicode.IsSpace`.
///
/// Rust's `str::trim` uses the Unicode `White_Space` property, which is the same set Go's
/// `unicode.IsSpace` consults, so this is a rename for the reader rather than a reimplementation.
fn go_trim_space(value: &str) -> &str {
    value.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_registration_request_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/client_registration_request.json");
        let v: ClientRegistrationRequest = serde_json::from_str(raw).expect("decodes");
        let ours: serde_json::Value = serde_json::to_value(&v).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("json");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn client_registration_response_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/client_registration_response.json");
        let v: ClientRegistrationResponse = serde_json::from_str(raw).expect("decodes");
        let ours: serde_json::Value = serde_json::to_value(&v).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("json");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn dcr_error_round_trips_the_fixture() {
        let raw = include_str!("../../../fixtures/dcr_error.json");
        let v: DcrError = serde_json::from_str(raw).expect("decodes");
        let ours: serde_json::Value = serde_json::to_value(&v).expect("re-encodes");
        let theirs: serde_json::Value = serde_json::from_str(raw).expect("json");
        assert_eq!(ours, theirs);
    }
}

/// Parity tests driven by `fixtures/behaviour_oauth_dcr.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_oauth_dcr.json")).unwrap()
    }

    #[test]
    fn constants_match_go() {
        let c = &oracle()["constants"];
        assert_eq!(
            c["DCRErrorInvalidRedirectURI"],
            DCR_ERROR_INVALID_REDIRECT_URI
        );
        assert_eq!(
            c["DCRErrorInvalidClientMetadata"],
            DCR_ERROR_INVALID_CLIENT_METADATA
        );
        assert_eq!(
            c["DCRErrorUnsupportedOperation"],
            DCR_ERROR_UNSUPPORTED_OPERATION
        );
        assert_eq!(
            c["GrantTypeAuthorizationCode"],
            GRANT_TYPE_AUTHORIZATION_CODE
        );
        assert_eq!(c["GrantTypeRefreshToken"], GRANT_TYPE_REFRESH_TOKEN);
        assert_eq!(c["ResponseTypeCode"], RESPONSE_TYPE_CODE);
    }

    #[test]
    fn defaults_match_go() {
        let d = &oracle()["defaults"];
        assert_eq!(
            serde_json::to_value(get_default_grant_types()).unwrap(),
            d["grant_types"]
        );
        assert_eq!(
            serde_json::to_value(get_default_response_types()).unwrap(),
            d["response_types"]
        );
    }

    #[test]
    fn new_dcr_error_matches_go() {
        let case = &oracle()["dcr_error"][0];
        let e = new_dcr_error("some_type", "some description");
        assert_eq!(e.error, case["error"].as_str().unwrap());
        assert_eq!(
            e.error_description,
            case["error_description"].as_str().unwrap()
        );
    }

    #[test]
    fn wire_format_is_byte_exact() {
        for case in oracle()["wire"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let expected = case["json"].as_str().unwrap();
            let ours = if name.starts_with("request") {
                let v: ClientRegistrationRequest = serde_json::from_str(expected).unwrap();
                crate::utils::go_json_marshal(&v).unwrap()
            } else if name.starts_with("response") {
                let v: ClientRegistrationResponse = serde_json::from_str(expected).unwrap();
                crate::utils::go_json_marshal(&v).unwrap()
            } else {
                let v: DcrError = serde_json::from_str(expected).unwrap();
                crate::utils::go_json_marshal(&v).unwrap()
            };
            assert_eq!(ours, expected, "wire mismatch for {name}");
        }
    }

    fn long(n: usize) -> String {
        "a".repeat(n)
    }

    fn request_for(name: &str) -> ClientRegistrationRequest {
        let ok = || Some(vec!["https://example.com/cb".to_owned()]);
        let mut r = ClientRegistrationRequest {
            redirect_uris: ok(),
            ..Default::default()
        };
        match name {
            "valid_minimal" => {}
            "no_redirect_uris" => r.redirect_uris = None,
            "empty_redirect_uris" => r.redirect_uris = Some(vec![]),
            "bad_redirect_uri" => r.redirect_uris = Some(vec!["not a url".to_owned()]),
            "second_redirect_uri_bad" => {
                r.redirect_uris = Some(vec!["https://ok.example.com".to_owned(), "nope".to_owned()])
            }
            "client_name_at_cap" => r.client_name = Some(long(64)),
            "client_name_over_cap" => r.client_name = Some(long(65)),
            "client_name_empty_pointer" => r.client_name = Some(String::new()),
            "client_uri_bad_format" => r.client_uri = Some("not a url".to_owned()),
            "client_uri_too_long_and_malformed" => {
                r.client_uri = Some(format!("not a url {}", long(300)))
            }
            "client_uri_too_long_but_valid" => {
                r.client_uri = Some(format!("https://example.com/{}", long(256)))
            }
            "client_uri_at_cap" => {
                r.client_uri = Some(format!(
                    "https://e.com/{}",
                    long(256 - "https://e.com/".len())
                ))
            }
            "auth_method_none" => {
                r.token_endpoint_auth_method =
                    Some(crate::oauth::external::CLIENT_AUTH_METHOD_NONE.to_owned())
            }
            "auth_method_secret_post" => {
                r.token_endpoint_auth_method =
                    Some(crate::oauth::external::CLIENT_AUTH_METHOD_CLIENT_SECRET_POST.to_owned())
            }
            "auth_method_unsupported" => {
                r.token_endpoint_auth_method = Some("client_secret_basic".to_owned())
            }
            "auth_method_empty_pointer" => r.token_endpoint_auth_method = Some(String::new()),
            other => panic!("unmapped: {other}"),
        }
        r
    }

    #[test]
    fn request_is_valid_matches_go() {
        for case in oracle()["request_valid"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let got = request_for(name).is_valid();
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
                    "{name}: detailed_error"
                );
            }
        }
    }

    /// Format is checked before length, so a value that is both reports format.
    #[test]
    fn client_uri_format_is_checked_before_length() {
        let malformed_and_long = request_for("client_uri_too_long_and_malformed")
            .is_valid()
            .expect_err("rejected");
        assert_eq!(
            malformed_and_long.id, "model.dcr.is_valid.client_uri_format.app_error",
            "format wins over length"
        );

        let valid_but_long = request_for("client_uri_too_long_but_valid")
            .is_valid()
            .expect_err("rejected");
        assert_eq!(
            valid_but_long.id, "model.dcr.is_valid.client_uri_length.app_error",
            "a well-formed over-long URI reports length"
        );
    }

    #[test]
    fn pattern_validity_matches_go() {
        for case in oracle()["pattern_valid"].as_array().unwrap() {
            let pattern = case["pattern"].as_str().unwrap();
            assert_eq!(
                is_valid_dcr_redirect_uri_pattern(pattern),
                case["valid"].as_bool().unwrap(),
                "pattern validity mismatch for {pattern:?}"
            );
        }
    }

    /// The whole allowlist matcher, 44 probes.
    #[test]
    fn glob_matching_matches_go() {
        for case in oracle()["glob"].as_array().unwrap() {
            let uri = case["uri"].as_str().unwrap();
            let pattern = case["pattern"].as_str().unwrap();
            assert_eq!(
                redirect_uri_matches_glob(uri, pattern),
                case["matches"].as_bool().unwrap(),
                "glob mismatch: {uri:?} against {pattern:?}"
            );
        }
    }

    /// The claims the corpus exists to protect, asserted individually so a regression names
    /// itself rather than pointing at a case number.
    #[test]
    fn the_security_relevant_glob_properties_hold() {
        // A host wildcard must not satisfy a path requirement.
        assert!(!redirect_uri_matches_glob(
            "https://example.com/evil",
            "https://*/cb"
        ));
        assert!(redirect_uri_matches_glob(
            "https://example.com/cb",
            "https://*/cb"
        ));

        // `*` stops at `/`; `**` does not.
        assert!(!redirect_uri_matches_glob(
            "https://example.com/a/b",
            "https://example.com/*"
        ));
        assert!(redirect_uri_matches_glob(
            "https://example.com/a/b",
            "https://example.com/**"
        ));

        // A pattern with no query requires a candidate with none.
        assert!(!redirect_uri_matches_glob(
            "https://example.com/cb?a=1",
            "https://example.com/cb"
        ));

        // An invalid pattern never matches.
        assert!(!redirect_uri_matches_glob("https://example.com/cb", "***"));

        // Bytes, not characters: `*` consumes both bytes of `é`.
        assert!(redirect_uri_matches_glob(
            "https://example.com/café",
            "https://example.com/caf*"
        ));
    }

    /// The systematic half of the corpus: 3,240 (URI, pattern) pairs crossing a small component
    /// alphabet against a small wildcard alphabet, with Go's answer for each.
    ///
    /// This is what [D-101] asked for. The hand-written probes above are adversarial but finite;
    /// this enumerates the space rather than imagining it, which is the treatment
    /// `IsValidHTTPURL` got in [D-003] and which a security boundary deserves.
    #[test]
    fn generated_glob_sweep_matches_go() {
        let oracle = oracle();
        let cases = oracle["glob_generated"].as_array().unwrap();

        let mut matched = 0usize;
        for case in cases {
            let uri = case["uri"].as_str().unwrap();
            let pattern = case["pattern"].as_str().unwrap();
            let expected = case["matches"].as_bool().unwrap();
            if expected {
                matched += 1;
            }
            assert_eq!(
                redirect_uri_matches_glob(uri, pattern),
                expected,
                "generated sweep mismatch: {uri:?} against {pattern:?}"
            );
        }

        // A sweep whose answers are all one value proves nothing — a matcher hard-coded to
        // `false` would pass 2,928 of 3,240 cases. Assert the corpus is genuinely mixed, and
        // that it is the size it should be.
        assert_eq!(cases.len(), 3_240, "45 URIs x 72 patterns");
        assert!(
            matched > 100 && matched < cases.len() - 100,
            "the sweep must contain both outcomes in quantity, got {matched} matches"
        );
    }

    #[test]
    fn allowlist_matches_go() {
        let allowlist_for = |name: &str| -> Vec<String> {
            let s = |v: &str| v.to_owned();
            match name {
                "nil_allowlist" | "empty_allowlist" => vec![],
                "blank_entries_only" => vec![s(""), s("   "), s("\t")],
                "single_match" | "entry_needs_trimming" => {
                    if name == "entry_needs_trimming" {
                        vec![s("  https://example.com/cb  ")]
                    } else {
                        vec![s("https://example.com/cb")]
                    }
                }
                "single_miss" => vec![s("https://example.com/cb")],
                "second_matches" => {
                    vec![s("https://a.example.com/cb"), s("https://b.example.com/cb")]
                }
                "blank_then_match" => vec![s(""), s("https://example.com/cb")],
                "invalid_pattern_then_match" => vec![s("***"), s("https://example.com/cb")],
                "only_invalid_patterns" => vec![s("***"), s("ftp://x")],
                "wildcard_entry" => vec![s("https://*.example.com/cb")],
                other => panic!("unmapped: {other}"),
            }
        };

        for case in oracle()["allowlist"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let uri = case["uri"].as_str().unwrap();
            assert_eq!(
                redirect_uri_matches_allowlist(uri, &allowlist_for(name)),
                case["allowed"].as_bool().unwrap(),
                "allowlist mismatch for {name}"
            );
        }
    }

    /// The permissive default and its sharp edge, asserted on their own.
    #[test]
    fn an_empty_allowlist_permits_and_a_blank_one_denies() {
        assert!(
            redirect_uri_matches_allowlist("https://anything.example.com/cb", &[]),
            "an empty allowlist means NO RESTRICTION — failing closed here breaks every \
             unconfigured deployment"
        );

        let blanks = vec![String::new(), "   ".to_owned(), "\t".to_owned()];
        assert!(
            !redirect_uri_matches_allowlist("https://example.com/cb", &blanks),
            "a list of blanks is not empty, so the restriction applies and nothing matches"
        );
    }
}
