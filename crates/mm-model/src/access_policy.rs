//! Port of `model/access_policy.go` — ABAC access-control policies.
//!
//! # Five validators, one per policy version, and they are **not** cumulative
//!
//! `IsValid` dispatches on `Version` to one of `v0.1`…`v0.5`, each a standalone function that
//! re-checks everything. They differ in ways that are easy to miss:
//!
//! - **v0.1** requires a channel policy to have rules *and* allows at most one import; **v0.2**
//!   drops both — a channel policy may have imports and no rules.
//! - **v0.3** adds the `permission` and `team` types, requires exactly one role on a `permission`
//!   policy, and bans `user.session` in a membership expression.
//! - **v0.4** adds per-role permission rules on **channel policies only**, each needing a unique
//!   trimmed name and one of the three channel roles; membership rules may not carry a role and
//!   may not share a rule entry with a permission action.
//! - **v0.5** is the plugin lane: the type is `<pluginID>:<resourceType>`, actions are
//!   plugin-defined and only format-checked, and scope must be empty.
//!
//! # The version string is `x/mod/semver`, which requires a leading `v`
//!
//! `semver.IsValid("0.3")` is **false**; only `v0.3` passes. That is a different parser from the
//! `Masterminds/semver` used in `manifest.go`, and the two disagree on exactly this point.

use serde::{Deserialize, Serialize};

use crate::manifest::is_valid_plugin_id;
use crate::property_field::PropertyField;
use crate::role::{CHANNEL_ADMIN_ROLE_ID, CHANNEL_GUEST_ROLE_ID, CHANNEL_USER_ROLE_ID};
use crate::serde_helpers::is_empty_str;
use crate::user::User;
use crate::utils::{AppError, AppResult, StringInterface, go_quote, is_valid_id};

pub const ACCESS_CONTROL_POLICY_TYPE_PARENT: &str = "parent";
pub const ACCESS_CONTROL_POLICY_TYPE_CHANNEL: &str = "channel";
pub const ACCESS_CONTROL_POLICY_TYPE_PERMISSION: &str = "permission";
pub const ACCESS_CONTROL_POLICY_TYPE_TEAM: &str = "team";

pub const MAX_POLICY_NAME_LENGTH: usize = 128;

/// Port of `model.PluginAccessControlPolicyTypeSeparator` (access_policy.go:28) — ownership is
/// encoded in the key itself (`mattermost-ai:agent`), so core treats plugin types opaquely.
pub const PLUGIN_ACCESS_CONTROL_POLICY_TYPE_SEPARATOR: &str = ":";
pub const MAX_PLUGIN_RESOURCE_TYPE_LENGTH: usize = 64;
/// Bounds the **whole** type to the width of the `AccessControlPolicies.Type` column. A valid
/// plugin id alone can exceed it, which is why the combined key is bounded here rather than
/// failing at insert time.
pub const MAX_POLICY_TYPE_LENGTH: usize = 128;
pub const MAX_POLICY_ACTION_LENGTH: usize = 64;

pub const ACCESS_CONTROL_POLICY_VERSION_V0_1: &str = "v0.1";
pub const ACCESS_CONTROL_POLICY_VERSION_V0_2: &str = "v0.2";
pub const ACCESS_CONTROL_POLICY_VERSION_V0_3: &str = "v0.3";
pub const ACCESS_CONTROL_POLICY_VERSION_V0_4: &str = "v0.4";
pub const ACCESS_CONTROL_POLICY_VERSION_V0_5: &str = "v0.5";

pub const ACCESS_CONTROL_POLICY_ACTION_MEMBERSHIP: &str = "membership";
pub const ACCESS_CONTROL_POLICY_ACTION_UPLOAD_FILE_ATTACHMENT: &str = "upload_file_attachment";
pub const ACCESS_CONTROL_POLICY_ACTION_DOWNLOAD_FILE_ATTACHMENT: &str = "download_file_attachment";

pub const ACCESS_CONTROL_POLICY_SCOPE_TEAM: &str = "team";

/// Port of `allowedActionsV0_3` (access_policy.go:56).
fn is_allowed_action_v0_3(action: &str) -> bool {
    matches!(
        action,
        ACCESS_CONTROL_POLICY_ACTION_MEMBERSHIP
            | ACCESS_CONTROL_POLICY_ACTION_UPLOAD_FILE_ATTACHMENT
            | ACCESS_CONTROL_POLICY_ACTION_DOWNLOAD_FILE_ATTACHMENT
    )
}

/// Port of `allowedChannelRolesV0_4` (access_policy.go:64).
fn is_allowed_channel_role_v0_4(role: &str) -> bool {
    matches!(
        role,
        CHANNEL_GUEST_ROLE_ID | CHANNEL_USER_ROLE_ID | CHANNEL_ADMIN_ROLE_ID
    )
}

/// Port of `model.IsPermissionAction` (access_policy.go:80) — the non-membership actions a v0.4
/// channel rule may govern.
pub fn is_permission_action(action: &str) -> bool {
    matches!(
        action,
        ACCESS_CONTROL_POLICY_ACTION_UPLOAD_FILE_ATTACHMENT
            | ACCESS_CONTROL_POLICY_ACTION_DOWNLOAD_FILE_ATTACHMENT
    )
}

/// Port of `policyActionRe` (access_policy.go:91) — `^[a-z0-9]+(_[a-z0-9]+)*$`.
///
/// Lower-case alphanumeric segments joined by **single** underscores. It rejects the `*` wildcard,
/// a leading or trailing underscore, and a doubled one.
fn matches_policy_action_pattern(action: &str) -> bool {
    if action.is_empty() {
        return false;
    }
    for segment in action.split('_') {
        if segment.is_empty()
            || !segment
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return false;
        }
    }
    true
}

/// Port of `model.IsValidPolicyAction` (access_policy.go:131).
///
/// Plugin policies name their own actions, so core checks the **format only**.
pub fn is_valid_policy_action(action: &str) -> bool {
    action.len() <= MAX_POLICY_ACTION_LENGTH && matches_policy_action_pattern(action)
}

/// Port of `pluginResourceTypeRe` (access_policy.go:86) — the same charset as a plugin id,
/// `^[a-zA-Z0-9-_\.]+$`.
fn matches_plugin_resource_type(resource_type: &str) -> bool {
    !resource_type.is_empty()
        && resource_type
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Port of `model.SplitPluginAccessControlPolicyType` (access_policy.go:96).
///
/// Splits on the **first** `:`. Both halves must be well formed, and the *whole* type is bounded
/// first — see [`MAX_POLICY_TYPE_LENGTH`].
pub fn split_plugin_access_control_policy_type(policy_type: &str) -> Option<(&str, &str)> {
    if policy_type.len() > MAX_POLICY_TYPE_LENGTH {
        return None;
    }
    let (plugin_id, resource_type) =
        policy_type.split_once(PLUGIN_ACCESS_CONTROL_POLICY_TYPE_SEPARATOR)?;
    if !is_valid_plugin_id(plugin_id) {
        return None;
    }
    if resource_type.len() > MAX_PLUGIN_RESOURCE_TYPE_LENGTH
        || !matches_plugin_resource_type(resource_type)
    {
        return None;
    }
    Some((plugin_id, resource_type))
}

/// Port of `model.IsPluginAccessControlPolicyType` (access_policy.go:112).
pub fn is_plugin_access_control_policy_type(policy_type: &str) -> bool {
    split_plugin_access_control_policy_type(policy_type).is_some()
}

/// Port of `model.PluginOwnsAccessControlPolicyType` (access_policy.go:123).
///
/// The comparison is **exact**: the stored type is matched byte-for-byte on delete and
/// evaluation, so accepting a case variant here would let a policy be read but never removed.
pub fn plugin_owns_access_control_policy_type(plugin_id: &str, policy_type: &str) -> bool {
    split_plugin_access_control_policy_type(policy_type)
        .is_some_and(|(owner, _)| owner == plugin_id)
}

/// `golang.org/x/mod/semver.IsValid` — **not** the `Masterminds` parser used by `manifest.go`.
///
/// Requires a leading `v`; the minor and patch components are optional. See the module docs.
pub fn is_valid_go_mod_semver(version: &str) -> bool {
    let Some(rest) = version.strip_prefix('v') else {
        return false;
    };

    // Build metadata first: it may contain `-`.
    let (rest, build) = match rest.split_once('+') {
        Some((rest, build)) => (rest, Some(build)),
        None => (rest, None),
    };
    let (core, prerelease) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };

    let mut parts = core.split('.');
    let Some(major) = parts.next() else {
        return false;
    };
    if !is_semver_number(major) {
        return false;
    }
    for part in parts.by_ref().take(2) {
        if !is_semver_number(part) {
            return false;
        }
    }
    if parts.next().is_some() {
        return false;
    }

    if let Some(pre) = prerelease {
        if !is_semver_identifiers(pre, true) {
            return false;
        }
    }
    if let Some(build) = build {
        if !is_semver_identifiers(build, false) {
            return false;
        }
    }

    true
}

fn is_semver_number(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|b| b.is_ascii_digit())
        && (part.len() == 1 || !part.starts_with('0'))
}

fn is_semver_identifiers(s: &str, is_prerelease: bool) -> bool {
    s.split('.').all(|id| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !(is_prerelease
                && id.len() > 1
                && id.starts_with('0')
                && id.bytes().all(|b| b.is_ascii_digit()))
    })
}

/// Port of `model.AccessControlAttribute` (access_policy.go:155).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlAttribute {
    #[serde(rename = "attribute")]
    pub attribute: PropertyField,

    #[serde(rename = "values")]
    pub values: Option<Vec<String>>,
}

/// Port of `model.AccessControlPolicyTestResponse` (access_policy.go:160) — who a policy would
/// admit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlPolicyTestResponse {
    #[serde(rename = "users")]
    pub users: Option<Vec<User>>,

    #[serde(rename = "total")]
    pub total: i64,
}

/// Port of `model.AccessControlPolicyCursor` (access_policy.go:187).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlPolicyCursor {
    #[serde(rename = "id")]
    pub id: String,
}

impl AccessControlPolicyCursor {
    /// Port of `(*AccessControlPolicyCursor).IsEmpty` (access_policy.go:733).
    pub fn is_empty(&self) -> bool {
        self.id.is_empty()
    }

    /// Port of `(*AccessControlPolicyCursor).IsValid` (access_policy.go:737) — returns a bare
    /// `error`, not an `*AppError`.
    pub fn is_valid(&self) -> Result<(), AccessPolicyError> {
        if self.is_empty() {
            return Ok(());
        }
        if !is_valid_id(&self.id) {
            return Err(AccessPolicyError::InvalidCursorId);
        }
        Ok(())
    }
}

/// Port of `model.GetAccessControlPolicyOptions` (access_policy.go:165).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GetAccessControlPolicyOptions {
    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "parent_id")]
    pub parent_id: String,

    #[serde(rename = "cursor")]
    pub cursor: AccessControlPolicyCursor,

    #[serde(rename = "limit")]
    pub limit: i64,
}

/// Port of `model.AccessControlPolicySearch` (access_policy.go:172).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlPolicySearch {
    #[serde(rename = "term")]
    pub term: String,

    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "parent_id")]
    pub parent_id: String,

    #[serde(rename = "ids")]
    pub ids: Option<Vec<String>>,

    #[serde(rename = "cursor")]
    pub cursor: AccessControlPolicyCursor,

    #[serde(rename = "limit")]
    pub limit: i64,

    #[serde(rename = "include_children")]
    pub include_children: bool,

    #[serde(rename = "active")]
    pub active: bool,

    #[serde(rename = "team_id")]
    pub team_id: String,

    /// The only two `omitempty` fields here.
    #[serde(rename = "scope", skip_serializing_if = "is_empty_str")]
    pub scope: String,

    #[serde(rename = "scope_id", skip_serializing_if = "is_empty_str")]
    pub scope_id: String,

    #[serde(rename = "actions")]
    pub actions: Option<Vec<String>>,
}

/// Port of `model.AccessControlPoliciesWithCount` (access_policy.go:191).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlPoliciesWithCount {
    #[serde(rename = "policies")]
    pub policies: Option<Vec<AccessControlPolicy>>,

    #[serde(rename = "total")]
    pub total: i64,
}

/// Port of `model.AccessControlPolicyRule` (access_policy.go:216).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlPolicyRule {
    #[serde(rename = "actions")]
    pub actions: Option<Vec<String>>,

    /// A CEL expression. **Never parsed here** — the PDP compiles it.
    #[serde(rename = "expression")]
    pub expression: String,

    /// Required and unique-within-policy for a v0.4 permission rule.
    #[serde(rename = "name", skip_serializing_if = "is_empty_str")]
    pub name: String,

    /// The channel role a v0.4 permission rule applies to. Membership rules must leave it empty.
    #[serde(rename = "role", skip_serializing_if = "is_empty_str")]
    pub role: String,
}

impl AccessControlPolicyRule {
    fn actions_slice(&self) -> &[String] {
        self.actions.as_deref().unwrap_or(&[])
    }
}

/// Port of `model.AccessControlPolicy` (access_policy.go:196).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlPolicy {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    /// One of the four core types, or `<pluginID>:<resourceType>` at v0.5.
    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "active")]
    pub active: bool,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "revision")]
    pub revision: i64,

    /// `v0.1` … `v0.5`, **with the `v`**.
    #[serde(rename = "version")]
    pub version: String,

    #[serde(rename = "roles")]
    pub roles: Option<Vec<String>>,

    /// Ids of parent policies whose rules are merged in.
    #[serde(rename = "imports")]
    pub imports: Option<Vec<String>>,

    #[serde(rename = "rules")]
    pub rules: Option<Vec<AccessControlPolicyRule>>,

    /// `""` (system) or `team`.
    #[serde(rename = "scope", skip_serializing_if = "is_empty_str")]
    pub scope: String,

    /// The team id when `scope` is `team`.
    #[serde(rename = "scope_id", skip_serializing_if = "is_empty_str")]
    pub scope_id: String,

    #[serde(rename = "props")]
    pub props: Option<StringInterface>,
}

impl AccessControlPolicy {
    fn rules_slice(&self) -> &[AccessControlPolicyRule] {
        self.rules.as_deref().unwrap_or(&[])
    }

    fn imports_slice(&self) -> &[String] {
        self.imports.as_deref().unwrap_or(&[])
    }

    fn roles_slice(&self) -> &[String] {
        self.roles.as_deref().unwrap_or(&[])
    }

    /// Port of `(*AccessControlPolicy).HasPermissionRuleAction` (access_policy.go:142).
    ///
    /// Used by the API layer to gate channel-scope policies behind the
    /// `ChannelPermissionPolicies` feature flag. Go returns false for a nil policy so callers
    /// need no nil check; an empty policy answers false here for the same reason.
    pub fn has_permission_rule_action(&self) -> bool {
        self.rules_slice()
            .iter()
            .any(|rule| rule.actions_slice().iter().any(|a| is_permission_action(a)))
    }

    /// Port of `(*AccessControlPolicy).IsValid` (access_policy.go:267).
    ///
    /// The scope is checked **only when one of the two scope fields is set**, so a fully unscoped
    /// policy skips `validate_scope` entirely.
    pub fn is_valid(&self) -> AppResult {
        if !self.scope.is_empty() || !self.scope_id.is_empty() {
            self.validate_scope()?;
        }

        match self.version.as_str() {
            ACCESS_CONTROL_POLICY_VERSION_V0_1 => self.access_policy_version_v0_1(),
            ACCESS_CONTROL_POLICY_VERSION_V0_2 => self.access_policy_version_v0_2(),
            ACCESS_CONTROL_POLICY_VERSION_V0_3 => self.access_policy_version_v0_3(),
            ACCESS_CONTROL_POLICY_VERSION_V0_4 => self.access_policy_version_v0_4(),
            ACCESS_CONTROL_POLICY_VERSION_V0_5 => self.access_policy_version_v0_5(),
            _ => Err(err("version", String::new())),
        }
    }

    /// Port of `(*AccessControlPolicy).validateScope` (access_policy.go:290).
    fn validate_scope(&self) -> AppResult {
        match self.scope.as_str() {
            "" => {
                if !self.scope_id.is_empty() {
                    return Err(err("scope_id_without_scope", String::new()));
                }
            }
            ACCESS_CONTROL_POLICY_SCOPE_TEAM => {
                if !is_valid_id(&self.scope_id) {
                    return Err(err("scope_id", String::new()));
                }
            }
            _ => return Err(err("scope", String::new())),
        }
        Ok(())
    }

    /// The five checks every core version opens with, in order.
    fn common_header_checks(&self, name_required_for_permission: bool) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("id", String::new()));
        }

        let name_required = self.type_ == ACCESS_CONTROL_POLICY_TYPE_PARENT
            || (name_required_for_permission
                && self.type_ == ACCESS_CONTROL_POLICY_TYPE_PERMISSION);
        if name_required && (self.name.is_empty() || self.name.len() > MAX_POLICY_NAME_LENGTH) {
            return Err(err("name", String::new()));
        }

        if self.revision < 0 {
            return Err(err("revision", String::new()));
        }

        if !is_valid_go_mod_semver(&self.version) {
            return Err(err("version", String::new()));
        }

        Ok(())
    }

    /// Port of `accessPolicyVersionV0_1` (access_policy.go:306).
    ///
    /// The channel branch is the strictest of any version: rules are **required**, and at most
    /// **one** import is allowed.
    fn access_policy_version_v0_1(&self) -> AppResult {
        if self.type_ != ACCESS_CONTROL_POLICY_TYPE_PARENT
            && self.type_ != ACCESS_CONTROL_POLICY_TYPE_CHANNEL
        {
            return Err(err("type", String::new()));
        }

        self.common_header_checks(false)?;

        match self.type_.as_str() {
            ACCESS_CONTROL_POLICY_TYPE_PARENT => {
                if self.rules_slice().is_empty() {
                    return Err(err("rules", String::new()));
                }
                if !self.imports_slice().is_empty() {
                    return Err(err("imports", String::new()));
                }
            }
            ACCESS_CONTROL_POLICY_TYPE_CHANNEL => {
                if self.rules_slice().is_empty() && self.imports_slice().is_empty() {
                    return Err(err("rules_imports", String::new()));
                }
                // Reachable only when there *are* imports, since the branch above already
                // rejected "neither".
                if self.rules_slice().is_empty() {
                    return Err(err("rules", String::new()));
                }
                if self.imports_slice().len() > 1 {
                    return Err(err("imports", String::new()));
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Port of `accessPolicyVersionV0_2` (access_policy.go:353) — as v0.1 without the
    /// channel-side rules and import-count rules.
    fn access_policy_version_v0_2(&self) -> AppResult {
        if self.type_ != ACCESS_CONTROL_POLICY_TYPE_PARENT
            && self.type_ != ACCESS_CONTROL_POLICY_TYPE_CHANNEL
        {
            return Err(err("type", String::new()));
        }

        self.common_header_checks(false)?;

        match self.type_.as_str() {
            ACCESS_CONTROL_POLICY_TYPE_PARENT => {
                if self.rules_slice().is_empty() {
                    return Err(err("rules", String::new()));
                }
                if !self.imports_slice().is_empty() {
                    return Err(err("imports", String::new()));
                }
            }
            // A guard rather than a nested `if`: with the condition false, control falls to the
            // catch-all arm, which is what the nested form did.
            ACCESS_CONTROL_POLICY_TYPE_CHANNEL
                if self.rules_slice().is_empty() && self.imports_slice().is_empty() =>
            {
                return Err(err("rules_imports", String::new()));
            }
            _ => {}
        }

        Ok(())
    }

    fn is_core_type(&self) -> bool {
        matches!(
            self.type_.as_str(),
            ACCESS_CONTROL_POLICY_TYPE_PARENT
                | ACCESS_CONTROL_POLICY_TYPE_CHANNEL
                | ACCESS_CONTROL_POLICY_TYPE_PERMISSION
                | ACCESS_CONTROL_POLICY_TYPE_TEAM
        )
    }

    /// The per-type structural checks shared by v0.3 and v0.4.
    fn type_structure_checks_v0_3(&self) -> AppResult {
        match self.type_.as_str() {
            ACCESS_CONTROL_POLICY_TYPE_PARENT => {
                if self.rules_slice().is_empty() {
                    return Err(err("rules", String::new()));
                }
                if !self.imports_slice().is_empty() {
                    return Err(err("imports", String::new()));
                }
            }
            ACCESS_CONTROL_POLICY_TYPE_CHANNEL | ACCESS_CONTROL_POLICY_TYPE_TEAM
                if self.rules_slice().is_empty() && self.imports_slice().is_empty() =>
            {
                return Err(err("rules_imports", String::new()));
            }
            ACCESS_CONTROL_POLICY_TYPE_PERMISSION => {
                if self.rules_slice().is_empty() && self.imports_slice().is_empty() {
                    return Err(err("rules_imports", String::new()));
                }
                // Exactly one role — hierarchy is resolved at the PDP, not here.
                if self.roles_slice().len() != 1 {
                    return Err(err("roles", String::new()));
                }
                for role in self.roles_slice() {
                    if role.trim().is_empty() {
                        return Err(err("roles", String::new()));
                    }
                }
                if !self.imports_slice().is_empty() {
                    return Err(err("imports", String::new()));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Port of `accessPolicyVersionV0_3` (access_policy.go:392).
    fn access_policy_version_v0_3(&self) -> AppResult {
        if !self.is_core_type() {
            return Err(err("type", String::new()));
        }

        self.common_header_checks(true)?;
        self.type_structure_checks_v0_3()?;

        for rule in self.rules_slice() {
            if rule.actions_slice().is_empty() {
                return Err(err("actions", "actions must not be empty".to_string()));
            }
            for action in rule.actions_slice() {
                if !is_allowed_action_v0_3(action) {
                    return Err(err("actions", format!("unrecognized action: {action}")));
                }
            }
            // A membership rule may not depend on session attributes, which change mid-session.
            if rule
                .actions_slice()
                .iter()
                .any(|a| a == ACCESS_CONTROL_POLICY_ACTION_MEMBERSHIP)
                && rule.expression.contains("user.session")
            {
                return Err(err("session_attribute_on_membership", String::new()));
            }
        }

        Ok(())
    }

    /// Port of `accessPolicyVersionV0_4` (access_policy.go:480).
    ///
    /// **The v0.3 `user.session` ban is not carried forward** — v0.4 re-implements the rule loop
    /// and drops that check. Reproduced.
    fn access_policy_version_v0_4(&self) -> AppResult {
        if !self.is_core_type() {
            return Err(err("type", String::new()));
        }

        self.common_header_checks(true)?;
        self.type_structure_checks_v0_3()?;

        let mut seen_names = std::collections::HashSet::new();
        for rule in self.rules_slice() {
            if rule.actions_slice().is_empty() {
                return Err(err("actions", "actions must not be empty".to_string()));
            }

            let mut has_membership = false;
            let mut has_permission = false;
            for action in rule.actions_slice() {
                if !is_allowed_action_v0_3(action) {
                    return Err(err("actions", format!("unrecognized action: {action}")));
                }
                if action == ACCESS_CONTROL_POLICY_ACTION_MEMBERSHIP {
                    has_membership = true;
                }
                if is_permission_action(action) {
                    has_permission = true;
                }
            }

            if has_membership && has_permission {
                return Err(err(
                    "actions.membership_combined",
                    "membership cannot be combined with other actions in the same rule".to_string(),
                ));
            }

            if has_permission && self.type_ != ACCESS_CONTROL_POLICY_TYPE_CHANNEL {
                return Err(err(
                    "actions.permission_type",
                    "permission action rules are only allowed on channel policies".to_string(),
                ));
            }

            if has_permission {
                // Trimmed once, so the emptiness, length and uniqueness checks share one view —
                // which is what makes `"Uploads"` and `"Uploads "` a duplicate rather than two
                // visually identical rules.
                let n = rule.name.trim();
                if n.is_empty() || n.len() > MAX_POLICY_NAME_LENGTH {
                    return Err(err(
                        "rule_name",
                        "permission rules require a non-empty name within the policy max length"
                            .to_string(),
                    ));
                }
                if !is_allowed_channel_role_v0_4(&rule.role) {
                    return Err(err(
                        "rule_role",
                        format!("invalid channel role: {}", go_quote(&rule.role)),
                    ));
                }
                if !seen_names.insert(n.to_string()) {
                    return Err(err(
                        "rule_name_unique",
                        format!("duplicate rule name: {}", go_quote(n)),
                    ));
                }
            }

            if has_membership && !rule.role.is_empty() {
                return Err(err(
                    "rule_role",
                    "membership rules must not have a role".to_string(),
                ));
            }
        }

        Ok(())
    }

    /// Port of `accessPolicyVersionV0_5` (access_policy.go:598) — the plugin lane.
    ///
    /// Note it rejects **any** scope, including a `team` one that `validate_scope` would have
    /// accepted — so the two run in sequence and the stricter wins.
    fn access_policy_version_v0_5(&self) -> AppResult {
        if !is_plugin_access_control_policy_type(&self.type_) {
            return Err(err("plugin_type", String::new()));
        }

        if !is_valid_id(&self.id) {
            return Err(err("id", String::new()));
        }

        // Always required here: no backing entity supplies a name for a plugin type.
        if self.name.is_empty() || self.name.len() > MAX_POLICY_NAME_LENGTH {
            return Err(err("name", String::new()));
        }

        if self.revision < 0 {
            return Err(err("revision", String::new()));
        }

        if !is_valid_go_mod_semver(&self.version) {
            return Err(err("version", String::new()));
        }

        if self.rules_slice().is_empty() {
            return Err(err("rules", String::new()));
        }

        if !self.imports_slice().is_empty() {
            return Err(err("imports", String::new()));
        }

        if !self.roles_slice().is_empty() {
            return Err(err("roles", String::new()));
        }

        if !self.scope.is_empty() || !self.scope_id.is_empty() {
            return Err(err("scope", String::new()));
        }

        for rule in self.rules_slice() {
            if rule.actions_slice().is_empty() {
                return Err(err("actions", "actions must not be empty".to_string()));
            }
            for action in rule.actions_slice() {
                if !is_valid_policy_action(action) {
                    return Err(err("actions", format!("malformed action: {action}")));
                }
            }
            if !rule.role.is_empty() {
                return Err(err(
                    "rule_role",
                    "plugin policy rules must not have a role".to_string(),
                ));
            }
            if rule.name.len() > MAX_POLICY_NAME_LENGTH {
                return Err(err(
                    "rule_name",
                    "rule name exceeds the policy max length".to_string(),
                ));
            }
        }

        Ok(())
    }

    /// Port of `(*AccessControlPolicy).Inherit` (access_policy.go:657).
    ///
    /// Each version inherits differently:
    ///
    /// - **v0.1** *replaces* `imports` with the parent and **rewrites every rule's expression**
    ///   to `policies.id_<own id>` — note it references the child's own id, not the parent's, and
    ///   note that `rules` is built into a local and, in Go, **never assigned back**. So a v0.1
    ///   inherit changes only `Imports`. Reproduced.
    /// - **v0.2** appends, rejecting a duplicate.
    /// - **v0.3** additionally requires the parent to be a `parent`-type v0.3 policy and forbids
    ///   `permission` on either side.
    /// - **v0.4** does the same for a v0.3-or-v0.4 parent and stages the change on a **probe
    ///   copy**, so a post-merge validation failure leaves the receiver untouched — a
    ///   transactional contract the other versions do not offer.
    pub fn inherit(&mut self, parent: &AccessControlPolicy) -> AppResult {
        match self.version.as_str() {
            ACCESS_CONTROL_POLICY_VERSION_V0_1 => {
                self.imports = Some(vec![parent.id.clone()]);
                // Go builds a rewritten `rules` slice here and discards it; the receiver's rules
                // are left as they were.
            }
            ACCESS_CONTROL_POLICY_VERSION_V0_2 => {
                if self.imports_slice().contains(&parent.id) {
                    return Err(inherit_err("already_imported", String::new()));
                }
                self.imports
                    .get_or_insert_with(Vec::new)
                    .push(parent.id.clone());
            }
            ACCESS_CONTROL_POLICY_VERSION_V0_3 => {
                if self.type_ == ACCESS_CONTROL_POLICY_TYPE_PERMISSION
                    || parent.type_ == ACCESS_CONTROL_POLICY_TYPE_PERMISSION
                {
                    return Err(inherit_err("permission", String::new()));
                }
                if parent.type_ != ACCESS_CONTROL_POLICY_TYPE_PARENT {
                    return Err(inherit_err(
                        "parent_type",
                        "imports must target a parent-type policy".to_string(),
                    ));
                }
                if parent.version != ACCESS_CONTROL_POLICY_VERSION_V0_3 {
                    return Err(inherit_err("version", String::new()));
                }
                if self.imports_slice().contains(&parent.id) {
                    return Err(inherit_err("already_imported", String::new()));
                }
                self.imports
                    .get_or_insert_with(Vec::new)
                    .push(parent.id.clone());
            }
            ACCESS_CONTROL_POLICY_VERSION_V0_4 => {
                if self.type_ == ACCESS_CONTROL_POLICY_TYPE_PERMISSION
                    || parent.type_ == ACCESS_CONTROL_POLICY_TYPE_PERMISSION
                {
                    return Err(inherit_err("permission", String::new()));
                }
                if parent.version != ACCESS_CONTROL_POLICY_VERSION_V0_3
                    && parent.version != ACCESS_CONTROL_POLICY_VERSION_V0_4
                {
                    return Err(inherit_err("version", String::new()));
                }
                if parent.type_ != ACCESS_CONTROL_POLICY_TYPE_PARENT {
                    return Err(inherit_err(
                        "parent_type",
                        "v0.4 imports must target a membership parent policy".to_string(),
                    ));
                }
                if self.imports_slice().contains(&parent.id) {
                    return Err(inherit_err("already_imported", String::new()));
                }

                // Staged on a probe so a validation failure leaves the receiver untouched.
                let mut new_imports = self.imports_slice().to_vec();
                new_imports.push(parent.id.clone());
                let mut probe = self.clone();
                probe.imports = Some(new_imports.clone());
                probe.is_valid()?;

                self.imports = Some(new_imports);
                return Ok(());
            }
            _ => return Err(inherit_err("version", String::new())),
        }

        self.is_valid()
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "AccessControlPolicy.IsValid",
        format!("model.access_policy.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

fn inherit_err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "AccessControlPolicy.Inherit",
        format!("model.access_policy.inherit.{field}.app_error"),
        None,
        details,
        400,
    ))
}

/// Port of `model.CELExpressionError` (access_policy.go:228) — a compile error from the PDP, with
/// a **1-based line and column** as CEL reports them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CELExpressionError {
    #[serde(rename = "line")]
    pub line: i64,

    #[serde(rename = "column")]
    pub column: i64,

    #[serde(rename = "message")]
    pub message: String,
}

/// Port of `model.AccessControlQueryResult` (access_policy.go:234).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlQueryResult {
    #[serde(rename = "matched_subject_ids")]
    pub matched_subject_ids: Option<Vec<String>>,
}

/// Port of `model.AccessControlPolicyActiveUpdate` (access_policy.go:239).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlPolicyActiveUpdate {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "active")]
    pub active: bool,
}

/// Port of `model.AccessControlPolicyActiveUpdateRequest` (access_policy.go:245).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessControlPolicyActiveUpdateRequest {
    #[serde(rename = "entries")]
    pub entries: Option<Vec<AccessControlPolicyActiveUpdate>>,

    #[serde(rename = "team_id", skip_serializing_if = "is_empty_str")]
    pub team_id: String,
}

/// The one non-`AppError` failure in this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AccessPolicyError {
    #[error("cursor id is invalid")]
    InvalidCursorId,
}

#[cfg(test)]
mod go_parity {
    use super::is_valid_go_mod_semver;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_go_stdlib.json"))
            .expect("behaviour_go_stdlib.json is generated by reference/dump")
    }

    /// `golang.org/x/mod/semver.IsValid` — the parser these five validators use, which is **not**
    /// the one `manifest.go` uses. The corpus is shared with `manifest.rs`'s test so the rows
    /// where the two disagree are visible in both places.
    #[test]
    fn mod_semver_is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["mod_semver_isvalid"].as_array().unwrap();
        assert!(cases.len() >= 30);
        for case in cases {
            let input = case["in"].as_str().unwrap();
            assert_eq!(
                is_valid_go_mod_semver(input),
                case["out"].as_bool().unwrap(),
                "semver.IsValid({input:?})"
            );
        }
    }

    /// The five versions `AccessControlPolicy` actually stores must all pass, or every policy
    /// fails validation.
    #[test]
    fn the_five_stored_versions_are_valid() {
        for version in [
            super::ACCESS_CONTROL_POLICY_VERSION_V0_1,
            super::ACCESS_CONTROL_POLICY_VERSION_V0_2,
            super::ACCESS_CONTROL_POLICY_VERSION_V0_3,
            super::ACCESS_CONTROL_POLICY_VERSION_V0_4,
            super::ACCESS_CONTROL_POLICY_VERSION_V0_5,
        ] {
            assert!(is_valid_go_mod_semver(version), "{version} must be valid");
        }
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
    fn access_control_attribute_round_trips_the_fixture() {
        assert_fixture_round_trips!(AccessControlAttribute, "access_control_attribute");
    }
    #[test]
    fn access_control_policy_test_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            AccessControlPolicyTestResponse,
            "access_control_policy_test_response"
        );
    }
    #[test]
    fn get_access_control_policy_options_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            GetAccessControlPolicyOptions,
            "get_access_control_policy_options"
        );
    }
    #[test]
    fn access_control_policy_search_round_trips_the_fixture() {
        assert_fixture_round_trips!(AccessControlPolicySearch, "access_control_policy_search");
    }
    #[test]
    fn access_control_policy_cursor_round_trips_the_fixture() {
        assert_fixture_round_trips!(AccessControlPolicyCursor, "access_control_policy_cursor");
    }
    #[test]
    fn access_control_policies_with_count_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            AccessControlPoliciesWithCount,
            "access_control_policies_with_count"
        );
    }
    #[test]
    fn access_control_policy_round_trips_the_fixture() {
        assert_fixture_round_trips!(AccessControlPolicy, "access_control_policy");
    }
    #[test]
    fn access_control_policy_rule_round_trips_the_fixture() {
        assert_fixture_round_trips!(AccessControlPolicyRule, "access_control_policy_rule");
    }
    #[test]
    fn cel_expression_error_round_trips_the_fixture() {
        assert_fixture_round_trips!(CELExpressionError, "cel_expression_error");
    }
    #[test]
    fn access_control_query_result_round_trips_the_fixture() {
        assert_fixture_round_trips!(AccessControlQueryResult, "access_control_query_result");
    }
    #[test]
    fn access_control_policy_active_update_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            AccessControlPolicyActiveUpdate,
            "access_control_policy_active_update"
        );
    }
    #[test]
    fn access_control_policy_active_update_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            AccessControlPolicyActiveUpdateRequest,
            "access_control_policy_active_update_request"
        );
    }
}

#[cfg(test)]
mod is_valid_go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// The corpus's base policy. Every case is this with one thing changed, exactly as in
    /// `reference/dump/behaviour_sweep_models.go` — the two lists are matched by name, so a
    /// divergence in either order fails loudly rather than silently comparing the wrong pair.
    fn policy(mut mutate: impl FnMut(&mut AccessControlPolicy)) -> AccessControlPolicy {
        let mut p = AccessControlPolicy {
            id: "p1a2b3c4d5e6f7g8h9i0j1k2l3".to_string(),
            name: "Engineering only".to_string(),
            type_: ACCESS_CONTROL_POLICY_TYPE_CHANNEL.to_string(),
            version: ACCESS_CONTROL_POLICY_VERSION_V0_3.to_string(),
            rules: Some(vec![AccessControlPolicyRule {
                actions: Some(vec![ACCESS_CONTROL_POLICY_ACTION_MEMBERSHIP.to_string()]),
                expression: r#"user.attributes.team == "Engineering""#.to_string(),
                ..Default::default()
            }]),
            ..Default::default()
        };
        mutate(&mut p);
        p
    }

    fn rule(actions: &[&str], expression: &str, name: &str, role: &str) -> AccessControlPolicyRule {
        AccessControlPolicyRule {
            actions: Some(actions.iter().map(|a| a.to_string()).collect()),
            expression: expression.to_string(),
            name: name.to_string(),
            role: role.to_string(),
        }
    }

    fn cases() -> Vec<(&'static str, AccessControlPolicy)> {
        let session_expr = r#"user.session.ip_range == "10.0.0.0/8""#;
        vec![
            ("v0_3_channel_ok", policy(|_| {})),
            (
                "unknown_version",
                policy(|p| p.version = "v9.9".to_string()),
            ),
            ("empty_version", policy(|p| p.version = String::new())),
            ("bad_id", policy(|p| p.id = "short".to_string())),
            ("negative_revision", policy(|p| p.revision = -1)),
            ("unknown_type", policy(|p| p.type_ = "nope".to_string())),
            (
                "v0_1_channel_rules_only",
                policy(|p| p.version = ACCESS_CONTROL_POLICY_VERSION_V0_1.to_string()),
            ),
            (
                "v0_1_channel_imports_only",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_1.to_string();
                    p.rules = None;
                    p.imports = Some(vec!["i1a2b3c4d5e6f7g8h9i0j1k2l3".to_string()]);
                }),
            ),
            (
                "v0_1_channel_two_imports",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_1.to_string();
                    p.imports = Some(vec![
                        "i1a2b3c4d5e6f7g8h9i0j1k2l3".to_string(),
                        "i2a2b3c4d5e6f7g8h9i0j1k2l3".to_string(),
                    ]);
                }),
            ),
            (
                "v0_2_channel_imports_only",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_2.to_string();
                    p.rules = None;
                    p.imports = Some(vec!["i1a2b3c4d5e6f7g8h9i0j1k2l3".to_string()]);
                }),
            ),
            (
                "v0_2_channel_neither",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_2.to_string();
                    p.rules = None;
                }),
            ),
            (
                "v0_3_parent_ok",
                policy(|p| p.type_ = ACCESS_CONTROL_POLICY_TYPE_PARENT.to_string()),
            ),
            (
                "v0_3_parent_no_name",
                policy(|p| {
                    p.type_ = ACCESS_CONTROL_POLICY_TYPE_PARENT.to_string();
                    p.name = String::new();
                }),
            ),
            (
                "v0_3_parent_with_imports",
                policy(|p| {
                    p.type_ = ACCESS_CONTROL_POLICY_TYPE_PARENT.to_string();
                    p.imports = Some(vec!["i1a2b3c4d5e6f7g8h9i0j1k2l3".to_string()]);
                }),
            ),
            ("v0_3_channel_no_name", policy(|p| p.name = String::new())),
            (
                "v0_3_permission_one_role",
                policy(|p| {
                    p.type_ = ACCESS_CONTROL_POLICY_TYPE_PERMISSION.to_string();
                    p.roles = Some(vec!["system_user".to_string()]);
                }),
            ),
            (
                "v0_3_permission_no_roles",
                policy(|p| p.type_ = ACCESS_CONTROL_POLICY_TYPE_PERMISSION.to_string()),
            ),
            (
                "v0_3_permission_two_roles",
                policy(|p| {
                    p.type_ = ACCESS_CONTROL_POLICY_TYPE_PERMISSION.to_string();
                    p.roles = Some(vec!["system_user".to_string(), "system_admin".to_string()]);
                }),
            ),
            (
                "v0_3_permission_blank_role",
                policy(|p| {
                    p.type_ = ACCESS_CONTROL_POLICY_TYPE_PERMISSION.to_string();
                    p.roles = Some(vec!["   ".to_string()]);
                }),
            ),
            (
                "v0_3_no_actions",
                policy(|p| {
                    if let Some(rules) = p.rules.as_mut() {
                        rules[0].actions = None;
                    }
                }),
            ),
            (
                "v0_3_unknown_action",
                policy(|p| {
                    if let Some(rules) = p.rules.as_mut() {
                        rules[0].actions = Some(vec!["teleport".to_string()]);
                    }
                }),
            ),
            (
                "v0_3_session_on_membership",
                policy(|p| {
                    if let Some(rules) = p.rules.as_mut() {
                        rules[0].expression = session_expr.to_string();
                    }
                }),
            ),
            (
                "v0_4_session_on_membership",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_4.to_string();
                    if let Some(rules) = p.rules.as_mut() {
                        rules[0].expression = session_expr.to_string();
                    }
                }),
            ),
            (
                "v0_4_permission_rule_ok",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_4.to_string();
                    p.rules = Some(vec![rule(
                        &[ACCESS_CONTROL_POLICY_ACTION_UPLOAD_FILE_ATTACHMENT],
                        "true",
                        "Uploads",
                        crate::role::CHANNEL_USER_ROLE_ID,
                    )]);
                }),
            ),
            (
                "v0_4_permission_rule_no_name",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_4.to_string();
                    p.rules = Some(vec![rule(
                        &[ACCESS_CONTROL_POLICY_ACTION_UPLOAD_FILE_ATTACHMENT],
                        "",
                        "",
                        crate::role::CHANNEL_USER_ROLE_ID,
                    )]);
                }),
            ),
            (
                "v0_4_permission_rule_bad_role",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_4.to_string();
                    p.rules = Some(vec![rule(
                        &[ACCESS_CONTROL_POLICY_ACTION_UPLOAD_FILE_ATTACHMENT],
                        "",
                        "Uploads",
                        "system_user",
                    )]);
                }),
            ),
            (
                "v0_4_duplicate_rule_names_after_trim",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_4.to_string();
                    p.rules = Some(vec![
                        rule(
                            &[ACCESS_CONTROL_POLICY_ACTION_UPLOAD_FILE_ATTACHMENT],
                            "",
                            "Uploads",
                            crate::role::CHANNEL_USER_ROLE_ID,
                        ),
                        rule(
                            &[ACCESS_CONTROL_POLICY_ACTION_DOWNLOAD_FILE_ATTACHMENT],
                            "",
                            "Uploads ",
                            crate::role::CHANNEL_ADMIN_ROLE_ID,
                        ),
                    ]);
                }),
            ),
            (
                "v0_4_membership_with_role",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_4.to_string();
                    if let Some(rules) = p.rules.as_mut() {
                        rules[0].role = crate::role::CHANNEL_USER_ROLE_ID.to_string();
                    }
                }),
            ),
            (
                "v0_4_membership_and_permission_in_one_rule",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_4.to_string();
                    if let Some(rules) = p.rules.as_mut() {
                        rules[0].actions = Some(vec![
                            ACCESS_CONTROL_POLICY_ACTION_MEMBERSHIP.to_string(),
                            ACCESS_CONTROL_POLICY_ACTION_UPLOAD_FILE_ATTACHMENT.to_string(),
                        ]);
                    }
                }),
            ),
            (
                "v0_4_permission_rule_on_parent",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_4.to_string();
                    p.type_ = ACCESS_CONTROL_POLICY_TYPE_PARENT.to_string();
                    p.rules = Some(vec![rule(
                        &[ACCESS_CONTROL_POLICY_ACTION_UPLOAD_FILE_ATTACHMENT],
                        "",
                        "Uploads",
                        crate::role::CHANNEL_USER_ROLE_ID,
                    )]);
                }),
            ),
            (
                "v0_5_ok",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_5.to_string();
                    p.type_ = "com.mattermost.ai:agent".to_string();
                    p.rules = Some(vec![rule(&["invoke_agent"], "true", "", "")]);
                }),
            ),
            (
                "v0_5_core_type",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_5.to_string();
                    p.rules = Some(vec![rule(&["invoke_agent"], "", "", "")]);
                }),
            ),
            (
                "v0_5_malformed_action",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_5.to_string();
                    p.type_ = "com.mattermost.ai:agent".to_string();
                    p.rules = Some(vec![rule(&["Invoke_Agent"], "", "", "")]);
                }),
            ),
            (
                "v0_5_wildcard_action",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_5.to_string();
                    p.type_ = "com.mattermost.ai:agent".to_string();
                    p.rules = Some(vec![rule(&["*"], "", "", "")]);
                }),
            ),
            (
                "v0_5_with_role",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_5.to_string();
                    p.type_ = "com.mattermost.ai:agent".to_string();
                    p.roles = Some(vec!["system_user".to_string()]);
                    p.rules = Some(vec![rule(&["invoke_agent"], "", "", "")]);
                }),
            ),
            (
                "v0_5_team_scope",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_5.to_string();
                    p.type_ = "com.mattermost.ai:agent".to_string();
                    p.scope = ACCESS_CONTROL_POLICY_SCOPE_TEAM.to_string();
                    p.scope_id = "t9j3xbb5irn39gk6k8e9l5efgh".to_string();
                    p.rules = Some(vec![rule(&["invoke_agent"], "", "", "")]);
                }),
            ),
            (
                "scope_id_without_scope",
                policy(|p| p.scope_id = "t9j3xbb5irn39gk6k8e9l5efgh".to_string()),
            ),
            (
                "team_scope_bad_id",
                policy(|p| {
                    p.scope = ACCESS_CONTROL_POLICY_SCOPE_TEAM.to_string();
                    p.scope_id = "short".to_string();
                }),
            ),
            (
                "unknown_scope",
                policy(|p| {
                    p.scope = "galaxy".to_string();
                    p.scope_id = "t9j3xbb5irn39gk6k8e9l5efgh".to_string();
                }),
            ),
            (
                "team_scope_ok",
                policy(|p| {
                    p.scope = ACCESS_CONTROL_POLICY_SCOPE_TEAM.to_string();
                    p.scope_id = "t9j3xbb5irn39gk6k8e9l5efgh".to_string();
                }),
            ),
        ]
    }

    /// `IsValid` is five independent version validators sharing one entry point. The corpus walks
    /// each one's own rules, and the interesting half is where a later version stops caring about
    /// something an earlier one enforced.
    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let expected = oracle["access_policy_is_valid"].as_array().unwrap();
        let actual = cases();
        assert_eq!(
            expected.len(),
            actual.len(),
            "corpus and cases must line up"
        );

        for (case, (name, p)) in expected.iter().zip(actual.iter()) {
            assert_eq!(
                case["name"].as_str().unwrap(),
                *name,
                "corpus order drifted"
            );
            assert_eq!(
                p.has_permission_rule_action(),
                case["has_permission_action"].as_bool().unwrap(),
                "HasPermissionRuleAction({name})"
            );
            match (p.is_valid(), case.get("error_id").and_then(|v| v.as_str())) {
                (Ok(()), None) => {}
                (Ok(()), Some(id)) => panic!("{name}: Go rejected with {id}, the port accepted"),
                (Err(e), None) => panic!("{name}: Go accepted, the port rejected with {}", e.id),
                (Err(e), Some(id)) => {
                    assert_eq!(e.id, id, "{name}");
                    assert_eq!(e.where_, case["error_where"].as_str().unwrap(), "{name}");
                    assert_eq!(
                        e.detailed_error,
                        case["error_details"].as_str().unwrap(),
                        "{name}"
                    );
                }
            }
        }
    }

    /// `Inherit` differs per version in what it does to the receiver, not only in what it
    /// rejects: v0.1 **replaces** the import list where v0.2+ appends. Both the error and the
    /// mutated receiver are asserted.
    #[test]
    fn inherit_matches_go() {
        let oracle = oracle();
        let expected = oracle["access_policy_inherit"].as_array().unwrap();

        let parent = |version: &str, type_: &str| {
            policy(|p| {
                p.id = "q1a2b3c4d5e6f7g8h9i0j1k2l3".to_string();
                p.type_ = type_.to_string();
                p.version = version.to_string();
            })
        };
        let v0_3 = ACCESS_CONTROL_POLICY_VERSION_V0_3;
        let parent_type = ACCESS_CONTROL_POLICY_TYPE_PARENT;
        let channel_type = ACCESS_CONTROL_POLICY_TYPE_CHANNEL;

        let mut actual: Vec<(&str, AccessControlPolicy, AccessControlPolicy)> = vec![
            (
                "v0_1",
                policy(|p| p.version = ACCESS_CONTROL_POLICY_VERSION_V0_1.to_string()),
                parent(ACCESS_CONTROL_POLICY_VERSION_V0_1, parent_type),
            ),
            (
                "v0_2",
                policy(|p| p.version = ACCESS_CONTROL_POLICY_VERSION_V0_2.to_string()),
                parent(ACCESS_CONTROL_POLICY_VERSION_V0_2, parent_type),
            ),
            (
                "v0_2_duplicate",
                policy(|p| {
                    p.version = ACCESS_CONTROL_POLICY_VERSION_V0_2.to_string();
                    p.imports = Some(vec!["q1a2b3c4d5e6f7g8h9i0j1k2l3".to_string()]);
                }),
                parent(ACCESS_CONTROL_POLICY_VERSION_V0_2, parent_type),
            ),
            ("v0_3_ok", policy(|_| {}), parent(v0_3, parent_type)),
            (
                "v0_3_parent_is_channel",
                policy(|_| {}),
                parent(v0_3, channel_type),
            ),
            (
                "v0_3_parent_wrong_version",
                policy(|_| {}),
                parent(ACCESS_CONTROL_POLICY_VERSION_V0_2, parent_type),
            ),
            (
                "v0_3_permission_child",
                policy(|p| {
                    p.type_ = ACCESS_CONTROL_POLICY_TYPE_PERMISSION.to_string();
                    p.roles = Some(vec!["system_user".to_string()]);
                }),
                parent(v0_3, parent_type),
            ),
            (
                "v0_4_ok",
                policy(|p| p.version = ACCESS_CONTROL_POLICY_VERSION_V0_4.to_string()),
                parent(ACCESS_CONTROL_POLICY_VERSION_V0_4, parent_type),
            ),
            (
                "v0_4_parent_v0_3",
                policy(|p| p.version = ACCESS_CONTROL_POLICY_VERSION_V0_4.to_string()),
                parent(v0_3, parent_type),
            ),
            (
                "v0_5_unsupported",
                policy(|p| p.version = ACCESS_CONTROL_POLICY_VERSION_V0_5.to_string()),
                parent(v0_3, parent_type),
            ),
        ];
        assert_eq!(
            expected.len(),
            actual.len(),
            "corpus and cases must line up"
        );

        for (case, (name, child, parent)) in expected.iter().zip(actual.iter_mut()) {
            assert_eq!(
                case["name"].as_str().unwrap(),
                *name,
                "corpus order drifted"
            );
            match (
                child.inherit(parent),
                case.get("error_id").and_then(|v| v.as_str()),
            ) {
                (Ok(()), None) => {}
                (Ok(()), Some(id)) => panic!("{name}: Go rejected with {id}, the port accepted"),
                (Err(e), None) => panic!("{name}: Go accepted, the port rejected with {}", e.id),
                (Err(e), Some(id)) => {
                    assert_eq!(e.id, id, "{name}");
                    assert_eq!(e.where_, case["error_where"].as_str().unwrap(), "{name}");
                    assert_eq!(
                        e.detailed_error,
                        case["error_details"].as_str().unwrap(),
                        "{name}"
                    );
                }
            }

            // Go's nil slice reaches the corpus as `null`, which is the same "no imports" the
            // port spells `None` — hence `unwrap_or_default` rather than a decode error.
            let imports: Vec<String> =
                serde_json::from_value(case["imports"].clone()).unwrap_or_default();
            assert_eq!(
                child.imports.clone().unwrap_or_default(),
                imports,
                "{name} imports"
            );
            let exprs: Vec<String> =
                serde_json::from_value(case["rule_expressions"].clone()).unwrap_or_default();
            let actual_exprs: Vec<String> = child
                .rules_slice()
                .iter()
                .map(|r| r.expression.clone())
                .collect();
            assert_eq!(actual_exprs, exprs, "{name} rules");
        }
    }
}
