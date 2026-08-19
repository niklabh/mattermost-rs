//! Port of `model/scheme.go` — the five wire types and the validation that decides which role
//! names a scheme may name.
//!
//! A scheme binds a set of roles to a team or a channel; `mm-store/src/team_store.rs` already
//! resolves scheme roles one layer down. With `permission.go` and `role.go` ported, this completes
//! the model layer of [D-094].

use serde::{Deserialize, Serialize};

use crate::role::{Role, is_valid_role_name};
use crate::utils::{FAKE_SETTING, StringInterface, is_valid_id};

/// Port of the scheme constants (scheme.go:13). All three caps count **bytes**.
pub const SCHEME_DISPLAY_NAME_MAX_LENGTH: usize = 128;
pub const SCHEME_NAME_MAX_LENGTH: usize = 64;
pub const SCHEME_DESCRIPTION_MAX_LENGTH: usize = 1024;

/// The four values `Scope` may take. Note these are lowercase and unrelated to `role.rs`'s
/// `ROLE_SCOPE_*`, which are capitalised — `"team"` here, `"Team"` there.
pub const SCHEME_SCOPE_TEAM: &str = "team";
pub const SCHEME_SCOPE_CHANNEL: &str = "channel";
pub const SCHEME_SCOPE_PLAYBOOK: &str = "playbook";
pub const SCHEME_SCOPE_RUN: &str = "run";

/// Port of `model.Scheme` (scheme.go:23).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scheme {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "scope")]
    pub scope: String,

    #[serde(rename = "default_team_admin_role")]
    pub default_team_admin_role: String,

    #[serde(rename = "default_team_user_role")]
    pub default_team_user_role: String,

    #[serde(rename = "default_channel_admin_role")]
    pub default_channel_admin_role: String,

    #[serde(rename = "default_channel_user_role")]
    pub default_channel_user_role: String,

    #[serde(rename = "default_team_guest_role")]
    pub default_team_guest_role: String,

    #[serde(rename = "default_channel_guest_role")]
    pub default_channel_guest_role: String,

    #[serde(rename = "default_playbook_admin_role")]
    pub default_playbook_admin_role: String,

    #[serde(rename = "default_playbook_member_role")]
    pub default_playbook_member_role: String,

    #[serde(rename = "default_run_admin_role")]
    pub default_run_admin_role: String,

    #[serde(rename = "default_run_member_role")]
    pub default_run_member_role: String,
}

/// Port of `model.SchemePatch` (scheme.go:178).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemePatch {
    #[serde(rename = "name")]
    pub name: Option<String>,

    #[serde(rename = "display_name")]
    pub display_name: Option<String>,

    #[serde(rename = "description")]
    pub description: Option<String>,
}

/// Port of `model.SchemeIDPatch` (scheme.go:192).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemeIDPatch {
    #[serde(rename = "scheme_id")]
    pub scheme_id: Option<String>,
}

/// Port of `model.SchemeConveyor` (scheme.go:203) — a scheme plus the roles it names, for the
/// config import/export path.
///
/// The field names are **not** the `Scheme` ones: `TeamAdmin` here carries what `Scheme` calls
/// `DefaultTeamAdminRole`, and only the `json:` tags line the two up. The tags are what matter, so
/// they are what the port matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemeConveyor {
    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "scope")]
    pub scope: String,

    #[serde(rename = "default_team_admin_role")]
    pub team_admin: String,

    #[serde(rename = "default_team_user_role")]
    pub team_user: String,

    #[serde(rename = "default_team_guest_role")]
    pub team_guest: String,

    #[serde(rename = "default_channel_admin_role")]
    pub channel_admin: String,

    #[serde(rename = "default_channel_user_role")]
    pub channel_user: String,

    #[serde(rename = "default_channel_guest_role")]
    pub channel_guest: String,

    #[serde(rename = "default_playbook_admin_role")]
    pub playbook_admin: String,

    #[serde(rename = "default_playbook_member_role")]
    pub playbook_member: String,

    #[serde(rename = "default_run_admin_role")]
    pub run_admin: String,

    #[serde(rename = "default_run_member_role")]
    pub run_member: String,

    /// `[]*Role` with no `omitempty`, so a nil slice is `null` on the wire and an empty one `[]`.
    #[serde(rename = "roles")]
    pub roles: Option<Vec<Role>>,
}

/// Port of `model.SchemeRoles` (scheme.go:239) — which of the three scheme roles a membership
/// carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemeRoles {
    #[serde(rename = "scheme_admin")]
    pub scheme_admin: bool,

    #[serde(rename = "scheme_user")]
    pub scheme_user: bool,

    #[serde(rename = "scheme_guest")]
    pub scheme_guest: bool,
}

impl Scheme {
    /// Port of `(*Scheme).Auditable` (scheme.go:43) — all eighteen fields.
    #[must_use]
    pub fn auditable(&self) -> StringInterface {
        let mut out = serde_json::Map::new();
        out.insert("id".into(), self.id.clone().into());
        out.insert("name".into(), self.name.clone().into());
        out.insert("display_name".into(), self.display_name.clone().into());
        out.insert("description".into(), self.description.clone().into());
        out.insert("create_at".into(), self.create_at.into());
        out.insert("update_at".into(), self.update_at.into());
        out.insert("delete_at".into(), self.delete_at.into());
        out.insert("scope".into(), self.scope.clone().into());
        out.insert(
            "default_team_admin_role".into(),
            self.default_team_admin_role.clone().into(),
        );
        out.insert(
            "default_team_user_role".into(),
            self.default_team_user_role.clone().into(),
        );
        out.insert(
            "default_channel_admin_role".into(),
            self.default_channel_admin_role.clone().into(),
        );
        out.insert(
            "default_channel_user_role".into(),
            self.default_channel_user_role.clone().into(),
        );
        out.insert(
            "default_team_guest_role".into(),
            self.default_team_guest_role.clone().into(),
        );
        out.insert(
            "default_channel_guest_role".into(),
            self.default_channel_guest_role.clone().into(),
        );
        out.insert(
            "default_playbook_admin_role".into(),
            self.default_playbook_admin_role.clone().into(),
        );
        out.insert(
            "default_playbook_member_role".into(),
            self.default_playbook_member_role.clone().into(),
        );
        out.insert(
            "default_run_admin_role".into(),
            self.default_run_admin_role.clone().into(),
        );
        out.insert(
            "default_run_member_role".into(),
            self.default_run_member_role.clone().into(),
        );
        out
    }

    /// Port of `(*Scheme).Sanitize` (scheme.go:67).
    ///
    /// Blanks **three** fields including the `Name`, where `Role::sanitize` blanks only two and
    /// leaves the name alone. The asymmetry is Go's.
    pub fn sanitize(&mut self) {
        self.name = FAKE_SETTING.to_owned();
        self.display_name = FAKE_SETTING.to_owned();
        self.description = FAKE_SETTING.to_owned();
    }

    /// Port of `(*Scheme).IsValid` (scheme.go:249) — the id, then everything
    /// [`Scheme::is_valid_for_create`] checks. Returns a bare `bool`; Go has no error to carry.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        is_valid_id(&self.id) && self.is_valid_for_create()
    }

    /// Port of `(*Scheme).IsValidForCreate` (scheme.go:257).
    ///
    /// **The scope-dependent branches are not symmetric, and the shape is counter-intuitive.**
    ///
    /// - The three **channel** roles are required for *every* scope, `team` and `run` included.
    /// - The team, playbook and run roles are validated **only** under the `team` scope. A scheme
    ///   scoped `playbook` may carry an empty or malformed `default_playbook_admin_role` — the
    ///   fields named for a scope are not checked by that scope.
    /// - The `channel` scope additionally requires the three **team** roles to be empty, but says
    ///   nothing about the playbook and run roles, which may be set to anything.
    ///
    /// Measured over a 203-cell grid (every scope against every single-field mutation) rather than
    /// read off the switch, because reading it produces a confident wrong answer for at least one
    /// scope.
    #[must_use]
    pub fn is_valid_for_create(&self) -> bool {
        if self.display_name.is_empty() || self.display_name.len() > SCHEME_DISPLAY_NAME_MAX_LENGTH
        {
            return false;
        }

        if !is_valid_scheme_name(&self.name) {
            return false;
        }

        if self.description.len() > SCHEME_DESCRIPTION_MAX_LENGTH {
            return false;
        }

        if !matches!(
            self.scope.as_str(),
            SCHEME_SCOPE_TEAM | SCHEME_SCOPE_CHANNEL | SCHEME_SCOPE_PLAYBOOK | SCHEME_SCOPE_RUN
        ) {
            return false;
        }

        if !is_valid_role_name(&self.default_channel_admin_role)
            || !is_valid_role_name(&self.default_channel_user_role)
            || !is_valid_role_name(&self.default_channel_guest_role)
        {
            return false;
        }

        if self.scope == SCHEME_SCOPE_TEAM
            && (!is_valid_role_name(&self.default_team_admin_role)
                || !is_valid_role_name(&self.default_team_user_role)
                || !is_valid_role_name(&self.default_team_guest_role)
                || !is_valid_role_name(&self.default_playbook_admin_role)
                || !is_valid_role_name(&self.default_playbook_member_role)
                || !is_valid_role_name(&self.default_run_admin_role)
                || !is_valid_role_name(&self.default_run_member_role))
        {
            return false;
        }

        if self.scope == SCHEME_SCOPE_CHANNEL
            && (!self.default_team_admin_role.is_empty()
                || !self.default_team_user_role.is_empty()
                || !self.default_team_guest_role.is_empty())
        {
            return false;
        }

        true
    }

    /// Port of `(*Scheme).Patch` (scheme.go:331) — three fields, each replaced only when present.
    /// A present-but-empty string *is* applied, which is what distinguishes `Some("")` from `None`.
    pub fn patch(&mut self, patch: &SchemePatch) {
        if let Some(display_name) = &patch.display_name {
            self.display_name.clone_from(display_name);
        }
        if let Some(name) = &patch.name {
            self.name.clone_from(name);
        }
        if let Some(description) = &patch.description {
            self.description.clone_from(description);
        }
    }
}

impl SchemePatch {
    /// Port of `(*SchemePatch).Auditable` (scheme.go:184) — the three pointers, `null` when absent.
    #[must_use]
    pub fn auditable(&self) -> StringInterface {
        let mut out = serde_json::Map::new();
        out.insert("name".into(), option_value(self.name.as_ref()));
        out.insert(
            "display_name".into(),
            option_value(self.display_name.as_ref()),
        );
        out.insert(
            "description".into(),
            option_value(self.description.as_ref()),
        );
        out
    }
}

impl SchemeIDPatch {
    /// Port of `(*SchemeIDPatch).Auditable` (scheme.go:196).
    #[must_use]
    pub fn auditable(&self) -> StringInterface {
        let mut out = serde_json::Map::new();
        out.insert("scheme_id".into(), option_value(self.scheme_id.as_ref()));
        out
    }
}

impl SchemeRoles {
    /// Port of `(*SchemeRoles).Auditable` (scheme.go:245).
    ///
    /// It returns an **empty map**: none of the three booleans reaches the audit log. Reproduced
    /// rather than corrected — an audit record that suddenly gained three fields would be a
    /// divergence in the audit stream, which is the one place a difference is hardest to notice.
    #[must_use]
    pub fn auditable(&self) -> StringInterface {
        serde_json::Map::new()
    }
}

impl SchemeConveyor {
    /// Port of `(*SchemeConveyor).Scheme` (scheme.go:221).
    ///
    /// Carries fourteen fields and drops three groups: the `roles` themselves, the id, and all
    /// three timestamps. The result is a scheme that has never been persisted, so
    /// [`Scheme::is_valid`] rejects it (no id) while [`Scheme::is_valid_for_create`] may accept it.
    #[must_use]
    pub fn scheme(&self) -> Scheme {
        Scheme {
            display_name: self.display_name.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            scope: self.scope.clone(),
            default_team_admin_role: self.team_admin.clone(),
            default_team_user_role: self.team_user.clone(),
            default_team_guest_role: self.team_guest.clone(),
            default_channel_admin_role: self.channel_admin.clone(),
            default_channel_user_role: self.channel_user.clone(),
            default_channel_guest_role: self.channel_guest.clone(),
            default_playbook_admin_role: self.playbook_admin.clone(),
            default_playbook_member_role: self.playbook_member.clone(),
            default_run_admin_role: self.run_admin.clone(),
            default_run_member_role: self.run_member.clone(),
            ..Default::default()
        }
    }
}

fn option_value(value: Option<&String>) -> serde_json::Value {
    value.map_or(serde_json::Value::Null, |v| v.clone().into())
}

/// Port of `model.IsValidSchemeName` (scheme.go:345).
///
/// Go builds `^[a-z0-9_]{2,64}$` — and **recompiles it on every call**, which is a performance
/// quirk rather than a semantic one. Two semantics are worth naming:
///
/// - The minimum length is **2**. `role.go`'s `IsValidRoleName` accepts a single character, so the
///   two "name" rules differ at exactly one input.
/// - Go's `$` anchors at end of **text**, not end of line — unlike PCRE it does not also match
///   before a trailing newline — so `"ab\n"` is rejected. The character class excludes `\n`
///   anyway, which is why this predicate is equivalent to the regex without needing one, and the
///   corpus probes the newline cases to establish that rather than assume it.
#[must_use]
pub fn is_valid_scheme_name(name: &str) -> bool {
    // Every accepted character is one byte, so the byte length and the regex's rune count agree.
    (2..=SCHEME_NAME_MAX_LENGTH).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(raw: &str) {
        let parsed: T = serde_json::from_str(raw).expect("fixture parses");
        let ours = serde_json::to_value(&parsed).expect("re-serialises");
        let theirs: Value = serde_json::from_str(raw).expect("fixture is JSON");
        assert_eq!(theirs, ours);
    }

    #[test]
    fn scheme_json_round_trip() {
        round_trip::<Scheme>(include_str!("../../../fixtures/scheme.json"));
    }

    #[test]
    fn scheme_patch_json_round_trip() {
        round_trip::<SchemePatch>(include_str!("../../../fixtures/scheme_patch.json"));
    }

    #[test]
    fn scheme_id_patch_json_round_trip() {
        round_trip::<SchemeIDPatch>(include_str!("../../../fixtures/scheme_id_patch.json"));
    }

    #[test]
    fn scheme_conveyor_json_round_trip() {
        round_trip::<SchemeConveyor>(include_str!("../../../fixtures/scheme_conveyor.json"));
    }

    #[test]
    fn scheme_roles_json_round_trip() {
        round_trip::<SchemeRoles>(include_str!("../../../fixtures/scheme_roles.json"));
    }

    /// The conveyor renames ten fields relative to `Scheme` and only the `json:` tags line them
    /// up, so the two types must serialise the role fields identically or an exported scheme
    /// re-imports into the wrong slots.
    #[test]
    fn conveyor_and_scheme_agree_on_their_shared_keys() {
        let scheme: Scheme =
            serde_json::from_str(include_str!("../../../fixtures/scheme.json")).expect("parses");
        let conveyor = SchemeConveyor {
            name: scheme.name.clone(),
            display_name: scheme.display_name.clone(),
            description: scheme.description.clone(),
            scope: scheme.scope.clone(),
            team_admin: scheme.default_team_admin_role.clone(),
            team_user: scheme.default_team_user_role.clone(),
            team_guest: scheme.default_team_guest_role.clone(),
            channel_admin: scheme.default_channel_admin_role.clone(),
            channel_user: scheme.default_channel_user_role.clone(),
            channel_guest: scheme.default_channel_guest_role.clone(),
            playbook_admin: scheme.default_playbook_admin_role.clone(),
            playbook_member: scheme.default_playbook_member_role.clone(),
            run_admin: scheme.default_run_admin_role.clone(),
            run_member: scheme.default_run_member_role.clone(),
            roles: None,
        };

        let a = serde_json::to_value(&scheme).expect("serialises");
        let b = serde_json::to_value(&conveyor).expect("serialises");
        for key in [
            "name",
            "display_name",
            "description",
            "scope",
            "default_team_admin_role",
            "default_team_user_role",
            "default_team_guest_role",
            "default_channel_admin_role",
            "default_channel_user_role",
            "default_channel_guest_role",
            "default_playbook_admin_role",
            "default_playbook_member_role",
            "default_run_admin_role",
            "default_run_member_role",
        ] {
            assert_eq!(a[key], b[key], "{key}");
        }
        // And the round trip through `scheme()` is lossless for exactly those fields.
        let mut back = conveyor.scheme();
        back.id.clone_from(&scheme.id);
        back.create_at = scheme.create_at;
        back.update_at = scheme.update_at;
        back.delete_at = scheme.delete_at;
        assert_eq!(back, scheme);
    }

    #[test]
    fn nil_roles_serialise_as_null() {
        let conveyor = SchemeConveyor::default();
        assert_eq!(
            serde_json::to_value(&conveyor).expect("serialises")["roles"],
            Value::Null
        );
    }

    /// Asserted against `fixtures/behaviour_scheme.json`, written by `reference/dump`.
    mod go_parity {
        use super::*;
        use std::sync::OnceLock;

        fn oracle() -> &'static Value {
            static ORACLE: OnceLock<Value> = OnceLock::new();
            ORACLE.get_or_init(|| {
                serde_json::from_str(include_str!("../../../fixtures/behaviour_scheme.json"))
                    .expect("behaviour_scheme.json parses")
            })
        }

        fn cases(key: &str) -> &'static Vec<Value> {
            oracle()[key]
                .as_array()
                .unwrap_or_else(|| panic!("{key} is an array"))
        }

        #[test]
        fn constants_match_go() {
            let c = &oracle()["constants"];
            assert_eq!(
                c["display_name_max_length"].as_u64(),
                Some(SCHEME_DISPLAY_NAME_MAX_LENGTH as u64)
            );
            assert_eq!(
                c["name_max_length"].as_u64(),
                Some(SCHEME_NAME_MAX_LENGTH as u64)
            );
            assert_eq!(
                c["description_max_length"].as_u64(),
                Some(SCHEME_DESCRIPTION_MAX_LENGTH as u64)
            );
            assert_eq!(c["scope_team"].as_str(), Some(SCHEME_SCOPE_TEAM));
            assert_eq!(c["scope_channel"].as_str(), Some(SCHEME_SCOPE_CHANNEL));
            assert_eq!(c["scope_playbook"].as_str(), Some(SCHEME_SCOPE_PLAYBOOK));
            assert_eq!(c["scope_run"].as_str(), Some(SCHEME_SCOPE_RUN));
        }

        #[test]
        fn is_valid_scheme_name_matches_go() {
            let all = cases("is_valid_scheme_name");
            assert!(
                all.len() > 140,
                "the enumerated corpus shrank: {}",
                all.len()
            );
            for case in all {
                let input = case["in"].as_str().expect("an input");
                assert_eq!(
                    is_valid_scheme_name(input),
                    case["valid"].as_bool().expect("a verdict"),
                    "IsValidSchemeName({input:?})"
                );
            }
        }

        /// The two "name" rules differ at exactly one input, and this is it: a scheme name must be
        /// at least two characters where a role name may be one.
        #[test]
        fn scheme_names_need_two_characters_role_names_need_one() {
            assert!(crate::role::is_valid_role_name("a"));
            assert!(!is_valid_scheme_name("a"));
            assert!(is_valid_scheme_name("ab"));

            let one_char = cases("is_valid_scheme_name")
                .iter()
                .find(|c| c["in"].as_str() == Some("a"))
                .expect("the corpus probes a one-character name");
            assert_eq!(one_char["valid"].as_bool(), Some(false));
        }

        /// Go's `$` is end-of-text, not end-of-line. A port using a PCRE-flavoured engine would
        /// accept `"ab\n"`, and the corpus is what says Go does not.
        #[test]
        fn a_trailing_newline_is_rejected() {
            for probe in ["ab\n", "\nab", "ab\ncd", "ab\r"] {
                let case = cases("is_valid_scheme_name")
                    .iter()
                    .find(|c| c["in"].as_str() == Some(probe))
                    .unwrap_or_else(|| panic!("the corpus probes {probe:?}"));
                assert_eq!(case["valid"].as_bool(), Some(false), "{probe:?}");
                assert!(!is_valid_scheme_name(probe), "{probe:?}");
            }
        }

        /// The whole 203-cell grid: every scope against every single-field mutation.
        #[test]
        fn is_valid_matches_go_across_every_scope() {
            let all = cases("is_valid");
            assert!(all.len() > 190, "the grid shrank: {}", all.len());
            for case in all {
                let scheme: Scheme = serde_json::from_value(case["scheme"].clone())
                    .expect("the corpus scheme parses");
                let label = format!(
                    "scope={:?} mutation={}",
                    case["scope"].as_str().unwrap_or_default(),
                    case["mutation"].as_str().unwrap_or_default()
                );
                assert_eq!(
                    scheme.is_valid(),
                    case["is_valid"].as_bool().expect("a verdict"),
                    "IsValid: {label}"
                );
                assert_eq!(
                    scheme.is_valid_for_create(),
                    case["is_valid_for_create"].as_bool().expect("a verdict"),
                    "IsValidForCreate: {label}"
                );
            }
        }

        /// The finding the grid produced, asserted directly so it survives a refactor: the fields
        /// named for the playbook and run scopes are validated **only** under the *team* scope.
        #[test]
        fn playbook_and_run_roles_are_only_checked_under_the_team_scope() {
            let cell = |scope: &str, mutation: &str| -> bool {
                cases("is_valid")
                    .iter()
                    .find(|c| {
                        c["scope"].as_str() == Some(scope)
                            && c["mutation"].as_str() == Some(mutation)
                    })
                    .unwrap_or_else(|| panic!("no cell for {scope}/{mutation}"))["is_valid_for_create"]
                    .as_bool()
                    .expect("a verdict")
            };

            // Under `team`, an empty or malformed playbook role is fatal.
            assert!(!cell(SCHEME_SCOPE_TEAM, "empty_playbook_admin_role"));
            assert!(!cell(SCHEME_SCOPE_TEAM, "bad_playbook_admin_role"));
            // Under `playbook` and `run` — the scopes those fields are named for — it is not.
            assert!(cell(SCHEME_SCOPE_PLAYBOOK, "empty_playbook_admin_role"));
            assert!(cell(SCHEME_SCOPE_PLAYBOOK, "bad_playbook_admin_role"));
            assert!(cell(SCHEME_SCOPE_RUN, "empty_run_member_role"));

            // The channel scope forbids the three team roles but permits playbook and run roles.
            assert!(!cell(SCHEME_SCOPE_CHANNEL, "set_team_admin_role"));
            assert!(cell(SCHEME_SCOPE_CHANNEL, "set_playbook_admin_role"));

            // And the three channel roles are required by every scope, including `run`.
            for scope in [
                SCHEME_SCOPE_TEAM,
                SCHEME_SCOPE_CHANNEL,
                SCHEME_SCOPE_PLAYBOOK,
                SCHEME_SCOPE_RUN,
            ] {
                assert!(!cell(scope, "empty_channel_admin_role"), "{scope}");
                assert!(!cell(scope, "empty_channel_guest_role"), "{scope}");
            }
        }

        /// `IsValidForCreate` never looks at the id — that is `IsValid`'s only additional check.
        #[test]
        fn is_valid_for_create_ignores_the_id() {
            for mutation in ["bad_id", "empty_id"] {
                for case in cases("is_valid") {
                    if case["mutation"].as_str() != Some(mutation) {
                        continue;
                    }
                    let scheme: Scheme = serde_json::from_value(case["scheme"].clone())
                        .expect("the corpus scheme parses");
                    let scope = case["scope"].as_str().unwrap_or_default();
                    assert_eq!(
                        scheme.is_valid_for_create(),
                        case["is_valid_for_create"].as_bool().expect("a verdict"),
                        "{mutation} under {scope}"
                    );
                    assert!(
                        !scheme.is_valid(),
                        "{mutation} under {scope}: IsValid must fail"
                    );
                }
            }
        }

        #[test]
        fn patch_matches_go() {
            for case in cases("patch") {
                let name = case["name"].as_str().expect("a case name");
                let auditable = case["patch_auditable"].as_object().expect("an object");
                let field = |key: &str| -> Option<String> {
                    auditable
                        .get(key)
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                };
                let patch = SchemePatch {
                    name: field("name"),
                    display_name: field("display_name"),
                    description: field("description"),
                };

                let mut scheme = Scheme {
                    id: "scheme1jbyqbtxbtqcgy3wa9tjh".to_owned(),
                    name: "custom_scheme".to_owned(),
                    display_name: "Custom Scheme".to_owned(),
                    description: "a description".to_owned(),
                    scope: SCHEME_SCOPE_CHANNEL.to_owned(),
                    create_at: 1_755_000_000_000,
                    default_channel_admin_role: "custom_channel_admin".to_owned(),
                    ..Default::default()
                };
                scheme.patch(&patch);

                let expected: Scheme =
                    serde_json::from_value(case["scheme"].clone()).expect("the corpus parses");
                assert_eq!(scheme, expected, "{name}");

                // The auditable map keeps absent fields as explicit nulls.
                let ours = patch.auditable();
                assert_eq!(ours.len(), auditable.len(), "{name}: auditable key count");
                for (key, value) in auditable {
                    assert_eq!(ours.get(key), Some(value), "{name}: auditable[{key}]");
                }
            }
        }

        #[test]
        fn auditable_matches_go() {
            let a = &oracle()["auditable"];
            let scheme: Scheme =
                serde_json::from_value(a["scheme_source"].clone()).expect("the corpus parses");
            let ours = scheme.auditable();
            let theirs = a["scheme"].as_object().expect("an object");
            assert_eq!(ours.len(), theirs.len());
            for (key, value) in theirs {
                assert_eq!(ours.get(key), Some(value), "auditable[{key}]");
            }

            // SchemeRoles audits nothing at all, however its booleans are set.
            let roles: SchemeRoles =
                serde_json::from_value(a["scheme_roles_value"].clone()).expect("the corpus parses");
            assert!(roles.scheme_admin && roles.scheme_user && roles.scheme_guest);
            assert_eq!(a["scheme_roles_len"].as_u64(), Some(0));
            assert!(roles.auditable().is_empty());
        }

        #[test]
        fn sanitize_matches_go() {
            for case in cases("sanitize") {
                let mut scheme = Scheme {
                    name: case["name_before"].as_str().expect("a name").to_owned(),
                    scope: case["scope_before"].as_str().expect("a scope").to_owned(),
                    ..Default::default()
                };
                scheme.sanitize();
                assert_eq!(scheme.name, case["name_after"].as_str().unwrap());
                assert_eq!(scheme.display_name, case["display_after"].as_str().unwrap());
                assert_eq!(scheme.description, case["desc_after"].as_str().unwrap());
                assert_eq!(scheme.scope, case["scope_after"].as_str().unwrap());
            }
        }

        /// `Scheme::sanitize` blanks the name; `Role::sanitize` does not. Asserted together so the
        /// asymmetry cannot be "tidied" into consistency.
        #[test]
        fn scheme_sanitize_blanks_the_name_but_role_sanitize_does_not() {
            let mut scheme = Scheme {
                name: "custom_scheme".to_owned(),
                ..Default::default()
            };
            scheme.sanitize();
            assert_eq!(scheme.name, FAKE_SETTING);

            let mut role = crate::role::Role {
                name: "custom_role".to_owned(),
                display_name: "Custom".to_owned(),
                ..Default::default()
            };
            role.sanitize();
            assert_eq!(role.name, "custom_role");
            assert_eq!(role.display_name, FAKE_SETTING);
        }

        #[test]
        fn conveyor_scheme_matches_go() {
            for case in cases("conveyor") {
                let conveyor: SchemeConveyor =
                    serde_json::from_value(case["conveyor"].clone()).expect("the corpus parses");
                let expected: Scheme =
                    serde_json::from_value(case["scheme"].clone()).expect("the corpus parses");
                assert_eq!(conveyor.scheme(), expected, "case {}", case["case"]);

                assert!(case["id_empty"].as_bool().expect("a flag"));
                assert!(case["timestamps_zero"].as_bool().expect("a flag"));
                // The roles are dropped, however many the conveyor carried.
                assert_eq!(
                    conveyor.roles.as_deref().unwrap_or_default().len() as u64,
                    case["role_count"].as_u64().expect("a count")
                );
            }
        }
    }
}
