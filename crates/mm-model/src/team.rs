//! Port of `model/team.go` (team.go:1–316).
//!
//! The first type to get a **complete** `IsValid`: every check it makes is now available,
//! `IsValidEmail` included. Contrast `User::is_valid`, still blocked on `IsValidLocale`
//! (see D-001/D-002 in `docs/TECH_DEBT.md`).
//!
//! # Deliberately not translated here
//!
//! - `Auditable`/`LogClone` are audit-log projections; they follow the audit layer.
//! - `ShallowCopy` is `#[derive(Clone)]`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::utils::{
    AppError, AppResult, RESERVED_NAMES, etag, get_millis, is_valid_alpha_num, is_valid_email,
    is_valid_id, new_id, sanitize_unicode,
};

// ---------------------------------------------------------------------------
// Constants (team.go:14-24)
// ---------------------------------------------------------------------------

pub const TEAM_OPEN: &str = "O";
pub const TEAM_INVITE: &str = "I";

pub const TEAM_ALLOWED_DOMAINS_MAX_LENGTH: usize = 500;
pub const TEAM_COMPANY_NAME_MAX_LENGTH: usize = 64;
pub const TEAM_DESCRIPTION_MAX_LENGTH: usize = 255;
pub const TEAM_DISPLAY_NAME_MAX_RUNES: usize = 64;
pub const TEAM_EMAIL_MAX_LENGTH: usize = 128;
pub const TEAM_NAME_MAX_LENGTH: usize = 64;
pub const TEAM_NAME_MIN_LENGTH: usize = 2;

/// access_policy.go:49. Borrowed until that file is translated — see D-005.
pub const ACCESS_CONTROL_POLICY_ACTION_MEMBERSHIP: &str = "membership";

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

fn bool_map_is_empty(m: &Option<HashMap<String, bool>>) -> bool {
    m.as_ref().is_none_or(HashMap::is_empty)
}

// ---------------------------------------------------------------------------
// Team
// ---------------------------------------------------------------------------

/// Port of `model.Team` (team.go:26).
///
/// Note the pointer fields — `scheme_id`, `group_constrained`, `policy_id` — carry **no**
/// `omitempty`, so a nil pointer serialises as `null` and the key is always present. Only
/// `last_team_icon_update`, `policy_actions` and `recommended` are omitted when empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Team {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "email")]
    pub email: String,

    #[serde(rename = "type")]
    pub team_type: String,

    #[serde(rename = "company_name")]
    pub company_name: String,

    #[serde(rename = "allowed_domains")]
    pub allowed_domains: String,

    #[serde(rename = "invite_id")]
    pub invite_id: String,

    #[serde(rename = "allow_open_invite")]
    pub allow_open_invite: bool,

    #[serde(
        rename = "last_team_icon_update",
        default,
        skip_serializing_if = "is_zero"
    )]
    pub last_team_icon_update: i64,

    #[serde(rename = "scheme_id")]
    pub scheme_id: Option<String>,

    #[serde(rename = "group_constrained")]
    pub group_constrained: Option<bool>,

    /// Data Retention policy. Unrelated to the access-control policy fields below.
    #[serde(rename = "policy_id")]
    pub policy_id: Option<String>,

    #[serde(rename = "cloud_limits_archived")]
    pub cloud_limits_archived: bool,

    /// Not persisted; derived by the store via `EXISTS` on `AccessControlPolicies`. A
    /// read-path signal only — enforce with [`Team::has_membership_policy_action`].
    #[serde(rename = "policy_enforced")]
    pub policy_enforced: bool,

    /// Hydrated lazily; `None` when not hydrated.
    #[serde(
        rename = "policy_actions",
        default,
        skip_serializing_if = "bool_map_is_empty"
    )]
    pub policy_actions: Option<HashMap<String, bool>>,

    #[serde(rename = "policy_is_active")]
    pub policy_is_active: bool,

    /// Not persisted; a transient per-viewer hint on public, policy-enforced teams the
    /// requesting user qualifies to join. Never set on private teams.
    #[serde(rename = "recommended", default, skip_serializing_if = "is_false")]
    pub recommended: bool,
}

/// Port of `model.TeamPatch` (team.go:90). No field carries `omitempty`, so every key is
/// present on the wire and `null` means "not patching this".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamPatch {
    #[serde(rename = "display_name")]
    pub display_name: Option<String>,
    #[serde(rename = "description")]
    pub description: Option<String>,
    #[serde(rename = "company_name")]
    pub company_name: Option<String>,
    #[serde(rename = "allowed_domains")]
    pub allowed_domains: Option<String>,
    #[serde(rename = "allow_open_invite")]
    pub allow_open_invite: Option<bool>,
    #[serde(rename = "group_constrained")]
    pub group_constrained: Option<bool>,
    #[serde(rename = "cloud_limits_archived")]
    pub cloud_limits_archived: Option<bool>,
}

/// Port of `model.Invites` (team.go:113).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invites {
    #[serde(rename = "invites")]
    pub invites: Vec<HashMap<String, String>>,
}

impl Invites {
    /// Port of `(*Invites).ToEmailList` (team.go:122).
    ///
    /// A missing `email` key yields `""`, matching Go's zero value for an absent map entry —
    /// the entry is kept, not skipped.
    pub fn to_email_list(&self) -> Vec<&str> {
        self.invites
            .iter()
            .map(|invite| invite.get("email").map_or("", String::as_str))
            .collect()
    }
}

/// Port of `model.TeamsWithCount` (team.go:117).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamsWithCount {
    #[serde(rename = "teams")]
    pub teams: Vec<Team>,
    #[serde(rename = "total_count")]
    pub total_count: i64,
}

/// Port of `model.TeamForExport` (team.go:108).
///
/// The embedded `Team` inlines its fields; `SchemeName` has **no** json tag, so Go falls back
/// to the Go field name verbatim — capital S included.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamForExport {
    #[serde(flatten)]
    pub team: Team,
    #[serde(rename = "SchemeName")]
    pub scheme_name: Option<String>,
}

impl Team {
    /// Port of `(*Team).HasPolicyAction` (team.go:59). Nil-safe in Go; `None`-safe here.
    pub fn has_policy_action(&self, action: &str) -> bool {
        self.policy_actions
            .as_ref()
            .is_some_and(|actions| actions.get(action).copied().unwrap_or(false))
    }

    /// Port of `(*Team).HasMembershipPolicyAction` (team.go:66).
    pub fn has_membership_policy_action(&self) -> bool {
        self.has_policy_action(ACCESS_CONTROL_POLICY_ACTION_MEMBERSHIP)
    }

    /// Port of `(*Team).IsGroupConstrained` (team.go:308).
    pub fn is_group_constrained(&self) -> bool {
        self.group_constrained.unwrap_or(false)
    }

    /// Port of `(*Team).Sanitize` (team.go:273).
    pub fn sanitize(&mut self) {
        self.email.clear();
        self.invite_id.clear();
    }

    /// Port of `(*Team).IsValid` (team.go:134).
    ///
    /// Checks run in Go's order, because the first failure decides the error id the client
    /// sees. Two ids are reused: both email checks return `...is_valid.email.app_error`, and
    /// the display-name check returns `...is_valid.name.app_error` while the *name* check
    /// returns `...is_valid.url.app_error` — Go's naming, not a transcription slip.
    pub fn is_valid(&self) -> AppResult {
        // The id failure is the only one that does not carry "id=..." in the details.
        if !is_valid_id(&self.id) {
            return Err(self.error("id", ""));
        }
        if self.create_at == 0 {
            return Err(self.error("create_at", &self.id));
        }
        if self.update_at == 0 {
            return Err(self.error("update_at", &self.id));
        }
        if self.email.len() > TEAM_EMAIL_MAX_LENGTH {
            return Err(self.error("email", &self.id));
        }
        if !self.email.is_empty() && !is_valid_email(&self.email) {
            return Err(self.error("email", &self.id));
        }

        let display_name_runes = self.display_name.chars().count();
        if display_name_runes == 0 || display_name_runes > TEAM_DISPLAY_NAME_MAX_RUNES {
            return Err(self.error("name", &self.id));
        }
        if self.name.len() > TEAM_NAME_MAX_LENGTH {
            return Err(self.error("url", &self.id));
        }
        if self.description.len() > TEAM_DESCRIPTION_MAX_LENGTH {
            return Err(self.error("description", &self.id));
        }
        if self.invite_id.is_empty() {
            return Err(self.error("invite_id", &self.id));
        }
        if is_reserved_team_name(&self.name) {
            return Err(self.error("reserved", &self.id));
        }
        if !is_valid_team_name(&self.name) {
            return Err(self.error("characters", &self.id));
        }
        if self.team_type != TEAM_OPEN && self.team_type != TEAM_INVITE {
            return Err(self.error("type", &self.id));
        }
        if self.company_name.len() > TEAM_COMPANY_NAME_MAX_LENGTH {
            return Err(self.error("company", &self.id));
        }
        if self.allowed_domains.len() > TEAM_ALLOWED_DOMAINS_MAX_LENGTH {
            return Err(self.error("domains", &self.id));
        }
        Ok(())
    }

    fn error(&self, field: &str, id: &str) -> Box<AppError> {
        let details = if id.is_empty() {
            String::new()
        } else {
            format!("id={id}")
        };
        Box::new(AppError::new(
            "Team.IsValid",
            format!("model.team.is_valid.{field}.app_error"),
            None,
            details,
            400,
        ))
    }

    /// Port of `(*Team).PreSave` (team.go:194).
    ///
    /// Unlike `User::PreSave`, `CreateAt` is overwritten unconditionally — an inbound
    /// `create_at` is discarded even when set.
    /// Port of `(*Team).Etag` (team.go:130).
    ///
    /// A zero team yields `<version>..0` — the empty id is an empty component, not a bug.
    pub fn etag(&self) -> String {
        etag(&[&self.id, &self.update_at])
    }

    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        self.create_at = get_millis();
        self.update_at = self.create_at;

        self.name = sanitize_unicode(&self.name);
        self.display_name = sanitize_unicode(&self.display_name);
        self.description = sanitize_unicode(&self.description);
        self.company_name = sanitize_unicode(&self.company_name);

        if self.invite_id.is_empty() {
            self.invite_id = new_id();
        }
    }

    /// Port of `(*Team).PreUpdate` (team.go:212).
    pub fn pre_update(&mut self) {
        self.update_at = get_millis();
        self.name = sanitize_unicode(&self.name);
        self.display_name = sanitize_unicode(&self.display_name);
        self.description = sanitize_unicode(&self.description);
        self.company_name = sanitize_unicode(&self.company_name);
    }

    /// Port of `(*Team).Patch` (team.go:278).
    pub fn patch(&mut self, patch: &TeamPatch) {
        if let Some(v) = &patch.display_name {
            self.display_name = v.clone();
        }
        if let Some(v) = &patch.description {
            self.description = v.clone();
        }
        if let Some(v) = &patch.company_name {
            self.company_name = v.clone();
        }
        if let Some(v) = &patch.allowed_domains {
            self.allowed_domains = v.clone();
        }
        if let Some(v) = patch.allow_open_invite {
            self.allow_open_invite = v;
        }
        // Go assigns the pointer itself here rather than dereferencing, but it only does so
        // when non-nil, so the observable result is the same.
        if patch.group_constrained.is_some() {
            self.group_constrained = patch.group_constrained;
        }
        if let Some(v) = patch.cloud_limits_archived {
            self.cloud_limits_archived = v;
        }
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Port of `model.IsReservedTeamName` (team.go:220).
///
/// `strings.Index(s, value) == 0` is a **prefix** test, not equality — so `administrators`,
/// `apiary` and `postmaster` are all reserved.
pub fn is_reserved_team_name(s: &str) -> bool {
    let s = crate::utils::go_to_lower(s);
    RESERVED_NAMES.iter().any(|value| s.starts_with(value))
}

/// Port of `model.IsValidTeamName` (team.go:232).
pub fn is_valid_team_name(s: &str) -> bool {
    is_valid_alpha_num(s) && s.len() >= TEAM_NAME_MIN_LENGTH
}

/// Port of `model.CleanTeamName` (team.go:246).
///
/// Two things here are easy to get wrong:
///
/// - The reserved-word step removes **every** occurrence of the word, not just the prefix
///   that triggered it (`strings.Replace(s, value, "", -1)`). `adminadmin` cleans to `""`.
/// - The fallback is `NewId()`, not `NewUsername()` — a bare 26-character id.
pub fn clean_team_name(s: &str) -> String {
    let mut s = crate::utils::go_to_lower(&s.replace(' ', "-"));

    for value in RESERVED_NAMES {
        if s.starts_with(value) {
            s = s.replace(value, "");
        }
    }

    s = s.trim().to_string();
    s.retain(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    let s = s.trim_matches('-').to_string();

    if is_valid_team_name(&s) { s } else { new_id() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils;

    fn fixture_team() -> Team {
        serde_json::from_str(include_str!("../../../fixtures/team.json")).unwrap()
    }

    /// A team that passes every `IsValid` branch, so each test can break exactly one thing.
    fn valid_team() -> Team {
        Team {
            id: utils::new_id(),
            create_at: 1_700_000_000_000,
            update_at: 1_700_000_000_000,
            display_name: "Core Team".into(),
            name: "core-team".into(),
            email: "team@example.com".into(),
            team_type: TEAM_OPEN.into(),
            invite_id: utils::new_id(),
            ..Default::default()
        }
    }

    #[test]
    fn team_matches_go_serialization() {
        let go = include_str!("../../../fixtures/team.json");
        let parsed: Team = serde_json::from_str(go).unwrap();
        let round_tripped = serde_json::to_value(&parsed).unwrap();
        let expected: serde_json::Value = serde_json::from_str(go).unwrap();
        assert_eq!(round_tripped, expected);
    }

    #[test]
    fn nil_pointers_serialise_as_null_not_omitted() {
        // scheme_id, group_constrained and policy_id have no omitempty in Go.
        let value = serde_json::to_value(Team::default()).unwrap();
        let object = value.as_object().unwrap();
        for key in ["scheme_id", "group_constrained", "policy_id"] {
            assert!(object.contains_key(key), "{key} must be present");
            assert!(value[key].is_null(), "{key} must be null");
        }
        // The three that do carry omitempty.
        for key in ["last_team_icon_update", "policy_actions", "recommended"] {
            assert!(!object.contains_key(key), "{key} must be omitted at zero");
        }
    }

    #[test]
    fn policy_actions_are_nil_safe() {
        let mut team = Team::default();
        assert!(!team.has_policy_action("membership"));
        assert!(!team.has_membership_policy_action());

        team.policy_actions = Some(HashMap::new());
        assert!(!team.has_policy_action("membership"), "empty map");

        team.policy_actions = Some(HashMap::from([("membership".to_string(), false)]));
        assert!(!team.has_membership_policy_action(), "present but false");

        team.policy_actions = Some(HashMap::from([("membership".to_string(), true)]));
        assert!(team.has_membership_policy_action());
    }

    #[test]
    fn group_constrained_is_none_safe() {
        let mut team = Team::default();
        assert!(!team.is_group_constrained());
        team.group_constrained = Some(false);
        assert!(!team.is_group_constrained());
        team.group_constrained = Some(true);
        assert!(team.is_group_constrained());
    }

    #[test]
    fn sanitize_clears_email_and_invite_id() {
        let mut team = fixture_team();
        team.sanitize();
        assert_eq!(team.email, "");
        assert_eq!(team.invite_id, "");
    }

    // -- IsValid, one test per branch ---------------------------------------

    fn assert_invalid(team: &Team, expected_id: &str) {
        let err = team
            .is_valid()
            .expect_err("expected this team to be invalid");
        assert_eq!(
            err.id,
            format!("model.team.is_valid.{expected_id}.app_error")
        );
        assert_eq!(err.status_code, 400);
    }

    #[test]
    fn is_valid_accepts_a_well_formed_team() {
        assert!(valid_team().is_valid().is_ok());
        // The generated fixture must also be a valid team.
        assert!(fixture_team().is_valid().is_ok());
    }

    #[test]
    fn is_valid_rejects_a_bad_id() {
        let mut team = valid_team();
        team.id = "too-short".into();
        assert_invalid(&team, "id");

        // The id branch is the only one with empty details.
        let err = team.is_valid().unwrap_err();
        assert_eq!(err.detailed_error, "");
    }

    #[test]
    fn is_valid_rejects_zero_timestamps() {
        let mut team = valid_team();
        team.create_at = 0;
        assert_invalid(&team, "create_at");
        assert_eq!(
            team.is_valid().unwrap_err().detailed_error,
            format!("id={}", team.id)
        );

        let mut team = valid_team();
        team.update_at = 0;
        assert_invalid(&team, "update_at");
    }

    #[test]
    fn is_valid_rejects_bad_email_two_ways() {
        // Over the length cap.
        let mut team = valid_team();
        team.email = format!("{}@example.com", "a".repeat(TEAM_EMAIL_MAX_LENGTH));
        assert_invalid(&team, "email");

        // Malformed.
        let mut team = valid_team();
        team.email = "not an email".into();
        assert_invalid(&team, "email");

        // Empty is allowed — the check is skipped entirely.
        let mut team = valid_team();
        team.email = String::new();
        assert!(team.is_valid().is_ok());
    }

    #[test]
    fn is_valid_rejects_bad_display_name() {
        let mut team = valid_team();
        team.display_name = String::new();
        assert_invalid(&team, "name");

        // Runes, not bytes: 64 multi-byte characters are fine, 65 are not.
        let mut team = valid_team();
        team.display_name = "é".repeat(TEAM_DISPLAY_NAME_MAX_RUNES);
        assert!(team.is_valid().is_ok(), "64 runes is the limit, inclusive");

        team.display_name = "é".repeat(TEAM_DISPLAY_NAME_MAX_RUNES + 1);
        assert_invalid(&team, "name");
    }

    #[test]
    fn is_valid_name_failure_uses_the_url_error_id() {
        // Go names the *name* length failure "url", and the *display name* failure "name".
        let mut team = valid_team();
        team.name = "a".repeat(TEAM_NAME_MAX_LENGTH + 1);
        assert_invalid(&team, "url");
    }

    #[test]
    fn is_valid_rejects_long_description() {
        let mut team = valid_team();
        team.description = "a".repeat(TEAM_DESCRIPTION_MAX_LENGTH + 1);
        assert_invalid(&team, "description");
    }

    #[test]
    fn is_valid_requires_an_invite_id() {
        let mut team = valid_team();
        team.invite_id = String::new();
        assert_invalid(&team, "invite_id");
    }

    #[test]
    fn is_valid_rejects_reserved_and_malformed_names() {
        let mut team = valid_team();
        team.name = "admin-team".into(); // reserved by prefix
        assert_invalid(&team, "reserved");

        let mut team = valid_team();
        team.name = "Bad_Name".into(); // uppercase and underscore
        assert_invalid(&team, "characters");
    }

    #[test]
    fn is_valid_rejects_unknown_type() {
        let mut team = valid_team();
        team.team_type = "X".into();
        assert_invalid(&team, "type");

        for team_type in [TEAM_OPEN, TEAM_INVITE] {
            let mut team = valid_team();
            team.team_type = team_type.into();
            assert!(team.is_valid().is_ok(), "{team_type} should be valid");
        }
    }

    #[test]
    fn is_valid_rejects_long_company_and_domains() {
        let mut team = valid_team();
        team.company_name = "a".repeat(TEAM_COMPANY_NAME_MAX_LENGTH + 1);
        assert_invalid(&team, "company");

        let mut team = valid_team();
        team.allowed_domains = "a".repeat(TEAM_ALLOWED_DOMAINS_MAX_LENGTH + 1);
        assert_invalid(&team, "domains");
    }

    // -- lifecycle ----------------------------------------------------------

    #[test]
    fn pre_save_fills_id_invite_id_and_timestamps() {
        let mut team = Team::default();
        team.pre_save();

        assert!(utils::is_valid_id(&team.id));
        assert!(utils::is_valid_id(&team.invite_id));
        assert!(team.create_at > 0);
        assert_eq!(team.update_at, team.create_at);
    }

    #[test]
    fn pre_save_overwrites_create_at_even_when_set() {
        // Differs from User::PreSave, which preserves a non-zero CreateAt.
        let mut team = valid_team();
        team.create_at = 12345;
        team.pre_save();
        assert_ne!(team.create_at, 12345);
    }

    #[test]
    fn pre_save_keeps_an_existing_id_and_invite_id() {
        let mut team = valid_team();
        let (id, invite_id) = (team.id.clone(), team.invite_id.clone());
        team.pre_save();
        assert_eq!(team.id, id);
        assert_eq!(team.invite_id, invite_id);
    }

    #[test]
    fn pre_save_and_pre_update_strip_unicode() {
        let mut team = Team {
            name: "te\u{202E}am".into(),
            display_name: "Te\u{FEFF}am".into(),
            description: "de\u{2028}sc".into(),
            company_name: "co\u{202A}mp".into(),
            ..Default::default()
        };
        team.pre_save();
        assert_eq!(team.name, "team");
        assert_eq!(team.display_name, "Team");
        assert_eq!(team.description, "desc");
        assert_eq!(team.company_name, "comp");

        let mut team = Team {
            name: "te\u{202E}am".into(),
            ..Default::default()
        };
        team.pre_update();
        assert_eq!(team.name, "team");
        assert!(team.update_at > 0);
    }

    // -- patch --------------------------------------------------------------

    #[test]
    fn patch_applies_only_present_fields() {
        let mut team = fixture_team();
        let original_name = team.name.clone();

        let patch = TeamPatch {
            display_name: Some("New Name".into()),
            allow_open_invite: Some(false),
            ..Default::default()
        };
        team.patch(&patch);

        assert_eq!(team.display_name, "New Name");
        assert!(!team.allow_open_invite);
        assert_eq!(team.name, original_name, "name is not patchable");
    }

    #[test]
    fn patch_can_set_false_and_empty() {
        let mut team = fixture_team();
        team.group_constrained = Some(true);

        let patch = TeamPatch {
            description: Some(String::new()),
            group_constrained: Some(false),
            cloud_limits_archived: Some(false),
            ..Default::default()
        };
        team.patch(&patch);

        assert_eq!(team.description, "");
        assert_eq!(team.group_constrained, Some(false));
        assert!(!team.cloud_limits_archived);
    }

    // -- free functions -----------------------------------------------------

    #[test]
    fn is_reserved_team_name_is_a_prefix_test() {
        assert!(is_reserved_team_name("admin"));
        assert!(
            is_reserved_team_name("administrators"),
            "prefix, not equality"
        );
        assert!(is_reserved_team_name("ADMIN"), "lowercased first");
        assert!(is_reserved_team_name("postmaster"), "starts with 'post'");
        assert!(!is_reserved_team_name("my-admin"));
        assert!(!is_reserved_team_name("core-team"));
        assert!(!is_reserved_team_name(""));
    }

    #[test]
    fn is_valid_team_name_needs_two_chars_and_alpha_num() {
        assert!(is_valid_team_name("ab"));
        assert!(is_valid_team_name("core-team"));
        assert!(!is_valid_team_name("a"), "one character");
        assert!(!is_valid_team_name(""));
        assert!(!is_valid_team_name("Ab"), "uppercase");
        assert!(!is_valid_team_name("-ab"), "leading hyphen");
    }

    #[test]
    fn clean_team_name_removes_every_occurrence_of_a_reserved_prefix() {
        // Triggered by the prefix, but Replace(-1) strips both occurrences, leaving "x" —
        // which is one character, so the result is not a valid team name and Go falls back
        // to NewId(). Verified against Go; the intuitive answer ("x") is wrong.
        let cleaned = clean_team_name("adminxadmin");
        assert_eq!(cleaned.len(), utils::ID_LENGTH);

        // "api" is a prefix of "apiary", so it is stripped and "ary" survives.
        assert_eq!(clean_team_name("apiary"), "ary");
        assert_eq!(clean_team_name("postmaster"), "master");

        // Not a prefix, so nothing is stripped.
        assert_eq!(clean_team_name("xadmin"), "xadmin");
    }

    #[test]
    fn clean_team_name_normalises_spaces_case_and_junk() {
        assert_eq!(clean_team_name("My Team"), "my-team");
        assert_eq!(clean_team_name("My  Team!!"), "my--team");
        assert_eq!(clean_team_name("--core-team--"), "core-team");
        assert_eq!(clean_team_name("Team_2024"), "team2024");
    }

    #[test]
    fn clean_team_name_falls_back_to_a_new_id() {
        // Everything is stripped, so the result is not a valid team name.
        let cleaned = clean_team_name("!!!");
        assert_eq!(cleaned.len(), utils::ID_LENGTH);
        assert!(utils::is_valid_id(&cleaned));

        // A single surviving character is also too short.
        let cleaned = clean_team_name("a");
        assert_eq!(cleaned.len(), utils::ID_LENGTH);
    }

    #[test]
    fn invites_to_email_list_keeps_missing_keys_as_empty() {
        let invites = Invites {
            invites: vec![
                HashMap::from([("email".to_string(), "a@example.com".to_string())]),
                HashMap::from([("name".to_string(), "no email key".to_string())]),
            ],
        };
        assert_eq!(invites.to_email_list(), vec!["a@example.com", ""]);
        assert!(Invites::default().to_email_list().is_empty());
    }

    #[test]
    fn team_for_export_uses_gos_untagged_field_name() {
        let export = TeamForExport {
            team: valid_team(),
            scheme_name: Some("scheme".into()),
        };
        let value = serde_json::to_value(&export).unwrap();
        // No json tag on SchemeName in Go, so the Go field name is the key verbatim.
        assert_eq!(value["SchemeName"], "scheme");
        // The embedded Team is inlined, not nested.
        assert!(value["id"].is_string());
        assert!(value.get("team").is_none());
    }
}

/// Differential tests against the real Go `team.go` functions.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_utils.json")).unwrap()
    }

    #[test]
    fn is_reserved_team_name_matches_go() {
        let oracle = oracle();
        for (input, want) in oracle["is_reserved_team_name"].as_object().unwrap() {
            assert_eq!(
                is_reserved_team_name(input),
                want.as_bool().unwrap(),
                "IsReservedTeamName({input:?})"
            );
        }
    }

    #[test]
    fn is_valid_team_name_matches_go() {
        let oracle = oracle();
        for (input, want) in oracle["is_valid_team_name"].as_object().unwrap() {
            assert_eq!(
                is_valid_team_name(input),
                want.as_bool().unwrap(),
                "IsValidTeamName({input:?})"
            );
        }
    }

    #[test]
    fn clean_team_name_matches_go() {
        let oracle = oracle();
        let cases = oracle["clean_team_name"].as_object().unwrap();
        assert!(!cases.is_empty());

        for (input, want) in cases {
            let want = want.as_str().unwrap();
            let got = clean_team_name(input);
            if want == "<newid>" {
                // Go fell back to NewId(), which is random; assert the shape instead.
                assert_eq!(got.len(), utils::ID_LENGTH, "CleanTeamName({input:?})");
                assert!(utils::is_valid_id(&got));
            } else {
                assert_eq!(got, want, "CleanTeamName({input:?})");
            }
        }
    }
}
