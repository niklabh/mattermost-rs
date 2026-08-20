//! Port of `model/session.go` (session.go:1–362).
//!
//! This is the Strangler Fig's critical path: `mm-api` and the Go server share one `Sessions`
//! table and must validate the same `MMAUTHTOKEN`. A divergence here does not produce a
//! cosmetic bug — it logs users out, or worse, fails to.
//!
//! # Deliberately not translated here
//!
//! - `Auditable` is an audit-log projection; it follows the audit layer.
//! - `CreateAt_` / `ExpiresAt_` / `LastActivityAt_` (session.go:298) widen `i64` to `float64`
//!   for a template engine. Reproducing them in Rust would only add a lossy conversion.
//! - `DeepCopy` is `#[derive(Clone)]`.

use serde::{Deserialize, Serialize};

use crate::team_member::TeamMember;
use crate::user::USER_ROLES_MAX_LENGTH;
// `parse_go_bool` because session props are written by several different code paths, so
// `strconv.ParseBool`'s wider accepted set (`1 t T TRUE True …`) is reachable in them.
use crate::utils::{
    AppError, AppResult, StringMap, get_millis, is_valid_id, new_id, parse_go_bool,
};

// ---------------------------------------------------------------------------
// Constants (session.go:15-40)
// ---------------------------------------------------------------------------

pub const SESSION_COOKIE_TOKEN: &str = "MMAUTHTOKEN";
pub const SESSION_COOKIE_USER: &str = "MMUSERID";
pub const SESSION_COOKIE_CSRF: &str = "MMCSRF";
pub const SESSION_COOKIE_CLOUD_URL: &str = "MMCLOUDURL";

pub const SESSION_CACHE_SIZE: usize = 35000;

pub const SESSION_PROP_PLATFORM: &str = "platform";
pub const SESSION_PROP_OS: &str = "os";
pub const SESSION_PROP_BROWSER: &str = "browser";
pub const SESSION_PROP_TYPE: &str = "type";
pub const SESSION_PROP_USER_ACCESS_TOKEN_ID: &str = "user_access_token_id";
pub const SESSION_PROP_IS_BOT: &str = "is_bot";
pub const SESSION_PROP_IS_BOT_VALUE: &str = "true";
pub const SESSION_PROP_OAUTH_APP_ID: &str = "oauth_app_id";
pub const SESSION_PROP_MATTERMOST_APP_ID: &str = "mattermost_app_id";
pub const SESSION_PROP_LAST_REMOVED_DEVICE_ID: &str = "last_removed_device_id";
pub const SESSION_PROP_LAST_REMOVED_VOIP_DEVICE_ID: &str = "last_removed_voip_device_id";
pub const SESSION_PROP_DEVICE_NOTIFICATION_DISABLED: &str = "device_notification_disabled";
pub const SESSION_PROP_MOBILE_VERSION: &str = "mobile_version";
pub const SESSION_PROP_IS_GUEST: &str = "is_guest";

pub const SESSION_TYPE_USER_ACCESS_TOKEN: &str = "UserAccessToken";
pub const SESSION_TYPE_CLOUD_KEY: &str = "CloudKey";
pub const SESSION_TYPE_REMOTECLUSTER_TOKEN: &str = "RemoteClusterToken";

/// 5 minutes, in milliseconds.
pub const SESSION_ACTIVITY_TIMEOUT: i64 = 1000 * 60 * 5;
/// 100 years, in hours.
pub const SESSION_USER_ACCESS_TOKEN_EXPIRY_HOURS: i64 = 100 * 365 * 24;

/// The CSRF prop key. Not a named constant in Go — the string literal appears inline at
/// session.go:286 and session.go:295.
pub const SESSION_PROP_CSRF: &str = "csrf";

/// saml.go:14-16. Borrowed until that file is translated — see D-005.
pub mod external {
    pub const USER_AUTH_SERVICE_IS_SAML: &str = "isSaml";
    pub const USER_AUTH_SERVICE_IS_MOBILE: &str = "isMobile";
    pub const USER_AUTH_SERVICE_IS_OAUTH: &str = "isOAuthUser";
    /// push_notification.go:13-14
    pub const PUSH_NOTIFY_APPLE_REACT_NATIVE: &str = "apple_rn";
    pub const PUSH_NOTIFY_ANDROID_REACT_NATIVE: &str = "android_rn";
}

use external::*;

/// Port of `standardDevicePlatforms` (session.go:311) — the allowlist for `DeviceId`.
fn standard_device_platforms() -> [&'static str; 3] {
    [
        PUSH_NOTIFY_APPLE_REACT_NATIVE,
        "apple_rnbeta",
        PUSH_NOTIFY_ANDROID_REACT_NATIVE,
    ]
}

/// Port of `voIPDevicePlatforms` (session.go:319) — the allowlist for `VoIPDeviceId`.
///
/// Android is deliberately absent upstream; it has no VoIP equivalent yet.
fn voip_device_platforms() -> [&'static str; 2] {
    [PUSH_NOTIFY_APPLE_REACT_NATIVE, "apple_rnbeta"]
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// Port of `model.Session` (session.go:68).
///
/// No field carries `omitempty`, so every key is always present. `props` and `team_members`
/// are nil-able Go reference types and therefore serialise as `null`, not `{}` / `[]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "token")]
    pub token: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "expires_at")]
    pub expires_at: i64,

    #[serde(rename = "last_activity_at")]
    pub last_activity_at: i64,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "device_id")]
    pub device_id: String,

    #[serde(rename = "voip_device_id")]
    pub voip_device_id: String,

    #[serde(rename = "roles")]
    pub roles: String,

    #[serde(rename = "is_oauth")]
    pub is_oauth: bool,

    #[serde(rename = "expired_notify")]
    pub expired_notify: bool,

    /// Nil in Go serialises as `null`, not `{}`.
    #[serde(rename = "props")]
    pub props: Option<StringMap>,

    /// `db:"-"` in Go — hydrated by the app layer, not the store.
    #[serde(rename = "team_members")]
    pub team_members: Option<Vec<TeamMember>>,

    /// `db:"-"`. True for local-mode sessions, which are unrestricted.
    #[serde(rename = "local")]
    pub local: bool,
}

/// Port of `model.MobileSessionMetadata` (session.go:45). No json tags in Go, so the wire
/// keys are the Go field names verbatim.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MobileSessionMetadata {
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Platform")]
    pub platform: String,
    #[serde(rename = "Count")]
    pub count: f64,
    #[serde(rename = "NotificationDisabled")]
    pub notification_disabled: String,
}

/// Port of `model.LoginOptions` (session.go:55). Internal to the login path, never
/// serialised — a struct purely so `DoLogin`'s signature stops changing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginOptions {
    pub device_id: String,
    pub voip_device_id: String,
    pub is_mobile: bool,
    pub is_oauth_user: bool,
    pub is_saml: bool,
}

impl Session {
    /// Port of `(*Session).IsUnrestricted` (session.go:103). Local-mode sessions bypass
    /// every permission check.
    pub fn is_unrestricted(&self) -> bool {
        self.local
    }

    /// Port of `(*Session).IsValid` (session.go:125).
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(self.error("id", false));
        }
        if !is_valid_id(&self.user_id) {
            return Err(self.error("user_id", false));
        }
        if self.create_at == 0 {
            return Err(self.error("create_at", false));
        }
        if self.roles.len() > USER_ROLES_MAX_LENGTH {
            // The only branch that carries details.
            return Err(self.error("roles_limit", true));
        }
        Ok(())
    }

    fn error(&self, field: &str, with_details: bool) -> Box<AppError> {
        let details = if with_details {
            format!("session_id={}", self.id)
        } else {
            String::new()
        };
        Box::new(AppError::new(
            "Session.IsValid",
            format!("model.session.is_valid.{field}.app_error"),
            None,
            details,
            400,
        ))
    }

    /// Port of `(*Session).PreSave` (session.go:146).
    ///
    /// `CreateAt` is overwritten unconditionally, as in `Team::pre_save` and unlike
    /// `User::PreSave`. `ExpiresAt` is **not** set here — the caller owns expiry.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }
        if self.token.is_empty() {
            self.token = new_id();
        }

        self.create_at = get_millis();
        self.last_activity_at = self.create_at;

        self.props.get_or_insert_with(StringMap::new);
    }

    /// Port of `(*Session).Sanitize` (session.go:163). Strips the token only — `props` may
    /// still hold the CSRF value.
    pub fn sanitize(&mut self) {
        self.token.clear();
    }

    /// Port of `(*Session).IsExpired` (session.go:167).
    ///
    /// A non-positive `ExpiresAt` means "never expires", and the comparison is strictly
    /// greater-than, so a session is not expired at the exact millisecond it expires.
    pub fn is_expired(&self) -> bool {
        if self.expires_at <= 0 {
            return false;
        }
        get_millis() > self.expires_at
    }

    /// Port of `(*Session).AddProp` (session.go:179). Creates the map when nil.
    pub fn add_prop(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.props
            .get_or_insert_with(StringMap::new)
            .insert(key.into(), value.into());
    }

    fn prop(&self, key: &str) -> Option<&str> {
        self.props.as_ref()?.get(key).map(String::as_str)
    }

    /// Port of `(*Session).GetTeamByTeamId` (session.go:187).
    pub fn get_team_by_team_id(&self, team_id: &str) -> Option<&TeamMember> {
        self.team_members
            .as_ref()?
            .iter()
            .find(|tm| tm.team_id == team_id)
    }

    /// Port of `(*Session).IsMobileApp` (session.go:197).
    pub fn is_mobile_app(&self) -> bool {
        !self.device_id.is_empty() || self.is_mobile()
    }

    /// Port of `(*Session).IsMobile` (session.go:201). An unparseable value is `false`.
    pub fn is_mobile(&self) -> bool {
        self.prop(USER_AUTH_SERVICE_IS_MOBILE)
            .and_then(parse_go_bool)
            .unwrap_or(false)
    }

    /// Port of `(*Session).IsSaml` (session.go:214).
    pub fn is_saml(&self) -> bool {
        self.prop(USER_AUTH_SERVICE_IS_SAML)
            .and_then(parse_go_bool)
            .unwrap_or(false)
    }

    /// Port of `(*Session).IsOAuthUser` (session.go:227).
    pub fn is_oauth_user(&self) -> bool {
        self.prop(USER_AUTH_SERVICE_IS_OAUTH)
            .and_then(parse_go_bool)
            .unwrap_or(false)
    }

    /// Port of `(*Session).IsBotUser` (session.go:240).
    ///
    /// Exact string equality against `"true"` — **not** `ParseBool`, so `"1"` and `"True"`
    /// are false here while they are true for [`Session::is_mobile`]. Faithful to Go.
    pub fn is_bot_user(&self) -> bool {
        self.prop(SESSION_PROP_IS_BOT) == Some(SESSION_PROP_IS_BOT_VALUE)
    }

    /// Port of `(*Session).IsUserAccessToken` (session.go:251).
    pub fn is_user_access_token(&self) -> bool {
        self.prop(SESSION_PROP_TYPE) == Some(SESSION_TYPE_USER_ACCESS_TOKEN)
    }

    /// Port of `(*Session).IsIntegration` (session.go:264).
    ///
    /// True for bots, personal access tokens and OAuth apps. Does **not** cover webhooks or
    /// slash commands.
    pub fn is_integration(&self) -> bool {
        self.is_bot_user() || self.is_user_access_token() || self.is_oauth
    }

    /// Port of `(*Session).IsSSOLogin` (session.go:268).
    pub fn is_sso_login(&self) -> bool {
        self.is_oauth_user() || self.is_saml()
    }

    /// Port of `(*Session).IsGuest` (session.go:272). Exact `"true"`, not `ParseBool`.
    pub fn is_guest(&self) -> bool {
        self.prop(SESSION_PROP_IS_GUEST) == Some("true")
    }

    /// Port of `(*Session).GetUserRoles` (session.go:280).
    pub fn get_user_roles(&self) -> Vec<&str> {
        self.roles.split_whitespace().collect()
    }

    /// Port of `(*Session).GenerateCSRF` (session.go:284). Stores and returns the token.
    pub fn generate_csrf(&mut self) -> String {
        let token = new_id();
        self.add_prop(SESSION_PROP_CSRF, token.clone());
        token
    }

    /// Port of `(*Session).GetCSRF` (session.go:290).
    pub fn get_csrf(&self) -> &str {
        self.prop(SESSION_PROP_CSRF).unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// Device ids
// ---------------------------------------------------------------------------

/// Port of `model.IsValidDeviceId` (session.go:327).
///
/// Requires the `"<platform>[-v<N>]:<token>"` shape with `<platform>` in `allowed`. The
/// `-v<N>` suffix is stripped only when it is terminal and `N` parses as a non-negative
/// integer — and Go's `Atoi` accepts a leading `+`, so `apple_rn-v+2:tok` strips too.
pub fn is_valid_device_id(device_id: &str, allowed: &[&str]) -> bool {
    let Some((mut platform, token)) = device_id.split_once(':') else {
        return false;
    };
    if token.is_empty() {
        return false;
    }
    if let Some(index) = platform.rfind("-v")
        && let Ok(version) = platform[index + 2..].parse::<i64>()
        && version >= 0
    {
        platform = &platform[..index];
    }
    allowed.contains(&platform)
}

/// Port of `model.IsValidStandardDeviceId` (session.go:340).
pub fn is_valid_standard_device_id(device_id: &str) -> bool {
    is_valid_device_id(device_id, &standard_device_platforms())
}

/// Port of `model.IsValidVoIPDeviceId` (session.go:344).
pub fn is_valid_voip_device_id(device_id: &str) -> bool {
    is_valid_device_id(device_id, &voip_device_platforms())
}

/// Port of `model.RedactDeviceId` (session.go:350).
///
/// Returns `"<platform>:<first-16>…"` for logs. Empty input yields `""`; malformed input
/// yields the prefix before the colon.
///
/// **Deliberate divergence, same class as `limit_bytes` (D-007):** Go slices the token at
/// exactly 16 bytes and can split a multi-byte character. This truncates at the nearest char
/// boundary at or below 16. Device tokens are ASCII in practice.
pub fn redact_device_id(device_id: &str) -> String {
    if device_id.is_empty() {
        return String::new();
    }
    let Some((platform, token)) = device_id.split_once(':') else {
        return device_id.to_string();
    };
    if token.is_empty() {
        return platform.to_string();
    }
    if token.len() <= 16 {
        return format!("{platform}:{token}");
    }
    let mut end = 16;
    while !token.is_char_boundary(end) {
        end -= 1;
    }
    format!("{platform}:{}\u{2026}", &token[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils;

    fn valid_session() -> Session {
        Session {
            id: utils::new_id(),
            user_id: utils::new_id(),
            create_at: 1_700_000_000_000,
            roles: "system_user".into(),
            ..Default::default()
        }
    }

    #[test]
    fn session_matches_go_serialization() {
        let go = include_str!("../../../fixtures/session.json");
        let parsed: Session = serde_json::from_str(go).unwrap();
        let round_tripped = serde_json::to_value(&parsed).unwrap();
        let expected: serde_json::Value = serde_json::from_str(go).unwrap();
        assert_eq!(round_tripped, expected);
    }

    #[test]
    fn nil_props_and_team_members_serialise_as_null() {
        let value = serde_json::to_value(Session::default()).unwrap();
        assert!(value["props"].is_null());
        assert!(value["team_members"].is_null());
        // No field in Session carries omitempty, so every key is present.
        assert_eq!(value.as_object().unwrap().len(), 14);
    }

    // -- IsValid ------------------------------------------------------------

    #[test]
    fn is_valid_covers_every_branch() {
        assert!(valid_session().is_valid().is_ok());

        let mut session = valid_session();
        session.id = "short".into();
        assert_eq!(
            session.is_valid().unwrap_err().id,
            "model.session.is_valid.id.app_error"
        );

        let mut session = valid_session();
        session.user_id = String::new();
        assert_eq!(
            session.is_valid().unwrap_err().id,
            "model.session.is_valid.user_id.app_error"
        );

        let mut session = valid_session();
        session.create_at = 0;
        assert_eq!(
            session.is_valid().unwrap_err().id,
            "model.session.is_valid.create_at.app_error"
        );
    }

    #[test]
    fn is_valid_roles_limit_is_the_only_branch_with_details() {
        let mut session = valid_session();
        session.roles = "a".repeat(USER_ROLES_MAX_LENGTH + 1);
        let err = session.is_valid().unwrap_err();
        assert_eq!(err.id, "model.session.is_valid.roles_limit.app_error");
        assert_eq!(err.detailed_error, format!("session_id={}", session.id));

        // Exactly at the limit is fine.
        session.roles = "a".repeat(USER_ROLES_MAX_LENGTH);
        assert!(session.is_valid().is_ok());
    }

    // -- lifecycle ----------------------------------------------------------

    #[test]
    fn pre_save_fills_id_token_and_timestamps() {
        let mut session = Session::default();
        session.pre_save();

        assert!(utils::is_valid_id(&session.id));
        assert!(utils::is_valid_id(&session.token));
        assert!(session.create_at > 0);
        assert_eq!(session.last_activity_at, session.create_at);
        assert!(session.props.is_some());
        assert_eq!(session.expires_at, 0, "PreSave does not set expiry");
    }

    #[test]
    fn pre_save_keeps_existing_id_and_token_but_overwrites_create_at() {
        let mut session = valid_session();
        session.token = "existing-token".into();
        let (id, token) = (session.id.clone(), session.token.clone());
        session.pre_save();

        assert_eq!(session.id, id);
        assert_eq!(session.token, token);
        assert_ne!(session.create_at, 1_700_000_000_000, "always overwritten");
    }

    #[test]
    fn sanitize_strips_only_the_token() {
        let mut session = valid_session();
        session.token = "secret".into();
        session.add_prop(SESSION_PROP_CSRF, "csrf-value");

        session.sanitize();
        assert_eq!(session.token, "");
        assert_eq!(session.get_csrf(), "csrf-value", "props are left alone");
    }

    #[test]
    fn is_expired_treats_non_positive_as_never() {
        let mut session = valid_session();
        session.expires_at = 0;
        assert!(!session.is_expired());

        session.expires_at = -1;
        assert!(!session.is_expired());

        session.expires_at = 1; // 1970
        assert!(session.is_expired());

        session.expires_at = utils::get_millis() + 60_000;
        assert!(!session.is_expired());
    }

    // -- props --------------------------------------------------------------

    #[test]
    fn add_prop_creates_the_map() {
        let mut session = Session::default();
        assert!(session.props.is_none());
        session.add_prop("k", "v");
        assert_eq!(session.props.as_ref().unwrap()["k"], "v");
    }

    #[test]
    fn csrf_round_trips() {
        let mut session = Session::default();
        assert_eq!(session.get_csrf(), "", "nil props");

        let token = session.generate_csrf();
        assert!(utils::is_valid_id(&token));
        assert_eq!(session.get_csrf(), token);
    }

    #[test]
    fn parse_bool_props_accept_gos_wider_set() {
        // Go's strconv.ParseBool, not Rust's "true"/"false" only.
        for truthy in ["1", "t", "T", "TRUE", "true", "True"] {
            let mut session = Session::default();
            session.add_prop(USER_AUTH_SERVICE_IS_MOBILE, truthy);
            assert!(session.is_mobile(), "{truthy:?} should parse as true");
        }
        for falsy in ["0", "f", "F", "FALSE", "false", "False"] {
            let mut session = Session::default();
            session.add_prop(USER_AUTH_SERVICE_IS_MOBILE, falsy);
            assert!(!session.is_mobile(), "{falsy:?} should parse as false");
        }
        // Unparseable and absent both yield false.
        for bad in ["yes", "", "TrUe", "2"] {
            let mut session = Session::default();
            session.add_prop(USER_AUTH_SERVICE_IS_MOBILE, bad);
            assert!(!session.is_mobile(), "{bad:?} should fall back to false");
        }
        assert!(!Session::default().is_mobile(), "absent prop");
    }

    #[test]
    fn is_bot_and_is_guest_use_exact_equality_not_parse_bool() {
        // The asymmetry is real: "1" is true for is_mobile but false for is_bot_user.
        let mut session = Session::default();
        session.add_prop(SESSION_PROP_IS_BOT, "1");
        assert!(!session.is_bot_user());
        session.add_prop(SESSION_PROP_IS_BOT, "True");
        assert!(!session.is_bot_user());
        session.add_prop(SESSION_PROP_IS_BOT, "true");
        assert!(session.is_bot_user());

        let mut session = Session::default();
        session.add_prop(SESSION_PROP_IS_GUEST, "1");
        assert!(!session.is_guest());
        session.add_prop(SESSION_PROP_IS_GUEST, "true");
        assert!(session.is_guest());
    }

    #[test]
    fn is_integration_covers_all_three_sources() {
        assert!(!Session::default().is_integration());

        let mut session = Session::default();
        session.add_prop(SESSION_PROP_IS_BOT, "true");
        assert!(session.is_integration());

        let mut session = Session::default();
        session.add_prop(SESSION_PROP_TYPE, SESSION_TYPE_USER_ACCESS_TOKEN);
        assert!(session.is_user_access_token() && session.is_integration());

        // Note: the struct field, not a prop.
        let session = Session {
            is_oauth: true,
            ..Default::default()
        };
        assert!(session.is_integration());
        assert!(!session.is_oauth_user(), "different thing entirely");
    }

    #[test]
    fn is_sso_login_and_is_mobile_app() {
        let mut session = Session::default();
        assert!(!session.is_sso_login());
        session.add_prop(USER_AUTH_SERVICE_IS_SAML, "true");
        assert!(session.is_sso_login());

        let mut session = Session::default();
        session.add_prop(USER_AUTH_SERVICE_IS_OAUTH, "t");
        assert!(session.is_sso_login());

        // is_mobile_app is true via device id even with no props.
        let session = Session {
            device_id: "apple_rn:token".into(),
            ..Default::default()
        };
        assert!(session.is_mobile_app() && !session.is_mobile());
    }

    #[test]
    fn get_team_by_team_id_finds_or_returns_none() {
        let wanted = utils::new_id();
        let session = Session {
            team_members: Some(vec![
                TeamMember {
                    team_id: utils::new_id(),
                    ..Default::default()
                },
                TeamMember {
                    team_id: wanted.clone(),
                    roles: "team_admin".into(),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        assert_eq!(
            session.get_team_by_team_id(&wanted).unwrap().roles,
            "team_admin"
        );
        assert!(session.get_team_by_team_id("nope").is_none());
        assert!(Session::default().get_team_by_team_id(&wanted).is_none());
    }

    #[test]
    fn is_unrestricted_tracks_local() {
        assert!(!Session::default().is_unrestricted());
        let session = Session {
            local: true,
            ..Default::default()
        };
        assert!(session.is_unrestricted());
    }
}

/// Differential tests against the real Go `session.go` functions.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_utils.json")).unwrap()
    }

    fn check(section: &str, predicate: impl Fn(&str) -> bool) {
        let oracle = oracle();
        let cases = oracle[section].as_object().unwrap();
        assert!(!cases.is_empty(), "{section} corpus is empty");
        for (input, want) in cases {
            assert_eq!(
                predicate(input),
                want.as_bool().unwrap(),
                "{section}({input:?})"
            );
        }
    }

    #[test]
    fn is_valid_standard_device_id_matches_go() {
        check("is_valid_standard_device_id", is_valid_standard_device_id);
    }

    #[test]
    fn is_valid_voip_device_id_matches_go() {
        check("is_valid_voip_device_id", is_valid_voip_device_id);
    }

    #[test]
    fn redact_device_id_matches_go() {
        let oracle = oracle();
        for (input, want) in oracle["redact_device_id"].as_object().unwrap() {
            assert_eq!(
                redact_device_id(input),
                want.as_str().unwrap(),
                "RedactDeviceId({input:?})"
            );
        }
    }

    #[test]
    fn session_bool_props_match_go() {
        // Go's strconv.ParseBool via Session.IsMobile, against every prop value in the corpus.
        let oracle = oracle();
        for (input, want) in oracle["session_is_mobile"].as_object().unwrap() {
            let mut session = Session::default();
            session.add_prop(USER_AUTH_SERVICE_IS_MOBILE, input.clone());
            assert_eq!(
                session.is_mobile(),
                want.as_bool().unwrap(),
                "IsMobile with prop {input:?}"
            );
        }
    }
}
