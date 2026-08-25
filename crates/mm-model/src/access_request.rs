//! Port of `model/access_request.go` — the ABAC Policy Decision Point's request, decision and
//! simulation shapes.
//!
//! # `Subject.Role` is a fallback that never goes away
//!
//! `RoleForScope` walks `ScopedRoles` first, and for the **system** scope falls back to the
//! deprecated `Role` field whenever no system-scoped entry exists — *including* when
//! `ScopedRoles` is non-empty but holds only channel entries. Populating `ScopedRoles` therefore
//! does not suppress the fallback, which is the opposite of what "deprecated" usually implies.
//!
//! # `Session` lives under the subject on purpose
//!
//! Everything in it is keyed to the requesting principal — the network they are on, the client
//! they use, whether their device is MDM-managed — so the subject stays the single source of
//! truth for "everything known about the requester at decision time". Policies reference it as
//! `user.session.<key>`, and Go notes that `SavePolicy` currently **rejects** rules referencing
//! it until the live PDP wiring lands, so a control cannot ship whose production behaviour
//! diverges from the simulator.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::{
    is_empty_str, is_false, is_none, is_none_or_empty_map, is_none_or_empty_vec, is_zero_i64,
};
use crate::user::User;
use crate::utils::{StringInterface, StringMap};

pub const ACCESS_CONTROL_SUBJECT_SCOPE_SYSTEM: &str = "system";
pub const ACCESS_CONTROL_SUBJECT_SCOPE_CHANNEL: &str = "channel";

/// Port of `model.ScopedRole` (access_request.go:16) — a role plus the scope it applies in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScopedRole {
    /// One of the two `ACCESS_CONTROL_SUBJECT_SCOPE_*` constants.
    #[serde(rename = "scope")]
    pub scope: String,

    /// `system_user`, `channel_admin`, …
    #[serde(rename = "role")]
    pub role: String,
}

/// Port of `model.Subject` (access_request.go:26) — the user or virtual entity a decision is
/// about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Subject {
    /// A user id, bot id, … scoped to `type`.
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "type")]
    pub type_: String,

    /// **Deprecated** in favour of `scoped_roles`, but still the system-scope fallback — see the
    /// module docs. Not `omitempty`, so it is always written.
    #[serde(rename = "role")]
    pub role: String,

    #[serde(rename = "scoped_roles", skip_serializing_if = "is_none_or_empty_vec")]
    pub scoped_roles: Option<Vec<ScopedRole>>,

    /// Custom profile attributes. Single- or multi-valued, primitive or complex — hence `any`.
    #[serde(rename = "attributes")]
    pub attributes: Option<StringInterface>,

    /// Per-session environmental attributes, referenced as `user.session.<key>`.
    #[serde(rename = "session", skip_serializing_if = "is_none_or_empty_map")]
    pub session: Option<StringInterface>,

    /// Exposed to CEL as `user.email`.
    #[serde(rename = "email", skip_serializing_if = "is_empty_str")]
    pub email: String,

    /// Exposed as **`user.verified`**, not `user.email_verified`.
    #[serde(rename = "email_verified", skip_serializing_if = "is_false")]
    pub email_verified: bool,

    /// Exposed as `user.isbot`.
    #[serde(rename = "is_bot", skip_serializing_if = "is_false")]
    pub is_bot: bool,

    /// Epoch milliseconds. Exposed as `user.createat`.
    #[serde(rename = "create_at", skip_serializing_if = "is_zero_i64")]
    pub create_at: i64,
}

impl Subject {
    fn scoped_roles_slice(&self) -> &[ScopedRole] {
        self.scoped_roles.as_deref().unwrap_or(&[])
    }

    /// Port of `(*Subject).RoleForScope` (access_request.go:90) — the **first** matching entry,
    /// with the legacy system fallback described in the module docs.
    pub fn role_for_scope(&self, scope: &str) -> &str {
        for sr in self.scoped_roles_slice() {
            if sr.scope == scope {
                return &sr.role;
            }
        }
        if scope == ACCESS_CONTROL_SUBJECT_SCOPE_SYSTEM {
            return &self.role;
        }
        ""
    }

    /// Port of `(*Subject).RolesForScope` (access_request.go:115).
    ///
    /// **No legacy fallback**, unlike `role_for_scope`. The PDP populates one entry per scope
    /// today, so this returns at most one element; it exists so multi-role-per-scope consumers
    /// have a stable accessor when that invariant is relaxed. Go returns nil for no match, which
    /// is an empty `Vec` here.
    pub fn roles_for_scope(&self, scope: &str) -> Vec<String> {
        self.scoped_roles_slice()
            .iter()
            .filter(|sr| sr.scope == scope)
            .map(|sr| sr.role.clone())
            .collect()
    }

    /// Port of `(*Subject).SetScopedRole` (access_request.go:143) — upsert one role per scope.
    ///
    /// Three behaviours worth keeping:
    ///
    /// - an existing entry is **replaced in place**, preserving its position, and any later
    ///   duplicates for the same scope are dropped;
    /// - an **empty role removes** every entry for the scope — the channel hot path relies on
    ///   that to clear a stale role from a cached subject;
    /// - an **empty scope is a no-op**.
    ///
    /// Go always allocates a fresh backing array so the method is safe on a `ScopedRoles` slice
    /// aliased with another subject. Rust's ownership makes that aliasing unrepresentable, but
    /// the rebuild is kept because it is also what produces the de-duplication.
    pub fn set_scoped_role(&mut self, scope: &str, role: &str) {
        if scope.is_empty() {
            return;
        }

        let mut updated = false;
        let mut out: Vec<ScopedRole> = Vec::with_capacity(self.scoped_roles_slice().len() + 1);
        for sr in self.scoped_roles_slice() {
            if sr.scope != scope {
                out.push(sr.clone());
                continue;
            }
            if role.is_empty() || updated {
                continue;
            }
            out.push(ScopedRole {
                scope: scope.to_string(),
                role: role.to_string(),
            });
            updated = true;
        }
        if !updated && !role.is_empty() {
            out.push(ScopedRole {
                scope: scope.to_string(),
                role: role.to_string(),
            });
        }
        self.scoped_roles = Some(out);
    }
}

/// Port of `model.SubjectCursor` (access_request.go:196).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubjectCursor {
    #[serde(rename = "target_id")]
    pub target_id: String,
}

/// Port of `model.SubjectSearchOptions` (access_request.go:166).
///
/// `Query` and `Args` are **pre-built SQL** produced by the access-control service for the
/// active database driver — this type carries them rather than building them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubjectSearchOptions {
    #[serde(rename = "term")]
    pub term: String,

    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "query")]
    pub query: String,

    #[serde(rename = "args")]
    pub args: Option<Vec<serde_json::Value>>,

    #[serde(rename = "limit")]
    pub limit: i64,

    #[serde(rename = "cursor")]
    pub cursor: SubjectCursor,

    #[serde(rename = "allow_inactive")]
    pub allow_inactive: bool,

    #[serde(rename = "ignore_count")]
    pub ignore_count: bool,

    /// A **channel id**, despite the plural name — used when syncing channel members. Tagged
    /// `exclude_members`, not `exclude_channel_members`.
    #[serde(rename = "exclude_members")]
    pub exclude_channel_members: String,

    /// Restricts the search to one user, for validation queries that only need to know whether
    /// that user matches.
    #[serde(rename = "subject_id")]
    pub subject_id: String,

    /// Strips native attribute predicates (`user.email`, `user.verified`, `user.isbot`,
    /// `user.createat`) before building SQL, so a self-inclusion check tests only the CPA parts.
    #[serde(rename = "exclude_native_attributes", skip_serializing_if = "is_false")]
    pub exclude_native_attributes: bool,

    /// **A privacy gate**: drops first and last name from the searched fields so a term query
    /// cannot probe real names when `ShowFullName` is off for a non-privileged caller. The zero
    /// value keeps full-name search, which is what privileged callers get.
    #[serde(rename = "exclude_full_names", skip_serializing_if = "is_false")]
    pub exclude_full_names: bool,
}

/// Port of `model.Resource` (access_request.go:201) — the target of a request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Resource {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "type")]
    pub type_: String,
}

/// Port of `model.AccessRequest` (access_request.go:211) — the PDP's input.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessRequest {
    #[serde(rename = "subject")]
    pub subject: Subject,

    #[serde(rename = "resource")]
    pub resource: Resource,

    #[serde(rename = "action")]
    pub action: String,

    #[serde(rename = "context", skip_serializing_if = "is_none_or_empty_map")]
    pub context: Option<StringInterface>,
}

/// Port of `model.AccessDecisionContextKeyReason` (access_request.go:220) — the AuthZEN
/// decision-context key the reason is reported under.
pub const ACCESS_DECISION_CONTEXT_KEY_REASON: &str = "reason";

/// Port of `model.AccessDecisionReasonNoPolicy` (access_request.go:229) — marks an allow as
/// **vacuous**: no policy governs the request, so the caller may apply its own defaults instead
/// of treating the allow as a grant.
pub const ACCESS_DECISION_REASON_NO_POLICY: &str = "no_policy";

/// Port of `model.AccessDecision` (access_request.go:234) — the OpenID AuthZEN evaluation
/// response: a boolean plus optional context.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessDecision {
    #[serde(rename = "decision")]
    pub decision: bool,

    #[serde(rename = "context", skip_serializing_if = "is_none_or_empty_map")]
    pub context: Option<StringInterface>,
}

impl AccessDecision {
    /// Port of `model.NewNoPolicyAccessDecision` (access_request.go:241) — the vacuous allow.
    pub fn new_no_policy() -> Self {
        let mut context = StringInterface::new();
        context.insert(
            ACCESS_DECISION_CONTEXT_KEY_REASON.to_string(),
            serde_json::Value::String(ACCESS_DECISION_REASON_NO_POLICY.to_string()),
        );
        Self {
            decision: true,
            context: Some(context),
        }
    }

    /// Port of `(AccessDecision).Reason` (access_request.go:250) — `""` when the context carries
    /// none, or carries a non-string.
    pub fn reason(&self) -> &str {
        self.context
            .as_ref()
            .and_then(|c| c.get(ACCESS_DECISION_CONTEXT_KEY_REASON))
            .and_then(|v| v.as_str())
            .unwrap_or("")
    }

    /// Port of `(AccessDecision).IsNoPolicy` (access_request.go:259).
    ///
    /// **A denial is never a no-policy fallback**, however it is labelled — so a contradictory
    /// response (`decision: false` carrying the `no_policy` reason) stays a deny.
    pub fn is_no_policy(&self) -> bool {
        self.decision && self.reason() == ACCESS_DECISION_REASON_NO_POLICY
    }
}

/// Port of `model.QueryExpressionParams` (access_request.go:263).
///
/// **`channelId` and `teamId` are camelCase** here, unlike every other id in this file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryExpressionParams {
    #[serde(rename = "expression")]
    pub expression: String,

    #[serde(rename = "term")]
    pub term: String,

    #[serde(rename = "limit")]
    pub limit: i64,

    #[serde(rename = "after")]
    pub after: String,

    #[serde(rename = "channelId", skip_serializing_if = "is_empty_str")]
    pub channel_id: String,

    #[serde(rename = "teamId", skip_serializing_if = "is_empty_str")]
    pub team_id: String,
}

/// Where a simulated deny originated. Four are real and five are **simulation-only synthetics**
/// the live PDP never produces — `no_applicable_policy`, `no_session_data`, `sibling_saved`,
/// `no_applicable_rule`, and the `peer_policy` reclassification.
///
/// The scope-privacy rule that runs through all of them: a blame entry at the draft's own scope
/// may carry the failing rule's CEL text, and a truly upper-scoped one may not — otherwise the
/// simulate UI would leak the expression of a policy outside the editing scope.
pub const POLICY_SIMULATION_BLAME_SOURCE_THIS_RULE: &str = "this_rule";
pub const POLICY_SIMULATION_BLAME_SOURCE_SIBLING_RULE: &str = "sibling_rule";
pub const POLICY_SIMULATION_BLAME_SOURCE_CHANNEL_POLICY: &str = "channel_policy";
/// Emitted for both genuinely higher-scoped policies **and** same-scope peers; the public server
/// reclassifies the latter to [`POLICY_SIMULATION_BLAME_SOURCE_PEER_POLICY`] before responding.
pub const POLICY_SIMULATION_BLAME_SOURCE_SYSTEM_PERMISSION: &str = "system_permission";
pub const POLICY_SIMULATION_BLAME_SOURCE_PEER_POLICY: &str = "peer_policy";
/// Synthetic: the draft does not apply to this user at all. Recorded as a **vacuous allow**.
pub const POLICY_SIMULATION_BLAME_SOURCE_NO_APPLICABLE_POLICY: &str = "no_applicable_policy";
/// Synthetic: the user has no cached session attributes but the rules reference
/// `user.session.*`. Also a vacuous allow, so the picker shows a neutral pill rather than a
/// misleading deny.
pub const POLICY_SIMULATION_BLAME_SOURCE_NO_SESSION_DATA: &str = "no_session_data";
/// Synthetic: the edited rule alone would have denied, but an OR-folded sibling allowed.
pub const POLICY_SIMULATION_BLAME_SOURCE_SIBLING_SAVED: &str = "sibling_saved";
/// Synthetic, and only under the `this_rule` evaluation scope: the edited rule is silent on this
/// subject.
pub const POLICY_SIMULATION_BLAME_SOURCE_NO_APPLICABLE_RULE: &str = "no_applicable_rule";

/// The per-blame verdict. **An empty `Outcome` means `deny`** — every blame entry predating the
/// field was a denier, and consumers filtering for deniers must accept both.
pub const POLICY_SIMULATION_BLAME_OUTCOME_DENY: &str = "deny";
pub const POLICY_SIMULATION_BLAME_OUTCOME_ALLOW: &str = "allow";

pub const POLICY_SIMULATION_EVALUATION_KIND_AND: &str = "and";
pub const POLICY_SIMULATION_EVALUATION_KIND_OR: &str = "or";
pub const POLICY_SIMULATION_EVALUATION_KIND_NOT: &str = "not";
pub const POLICY_SIMULATION_EVALUATION_KIND_COMPARE: &str = "compare";
pub const POLICY_SIMULATION_EVALUATION_KIND_FUNCTION: &str = "function";
/// The catch-all for shapes the simulator does not decompose.
pub const POLICY_SIMULATION_EVALUATION_KIND_OTHER: &str = "other";

/// The three-way truth result of CEL evaluation — note these are **strings**, not booleans, so
/// `error` is expressible.
pub const POLICY_SIMULATION_EVALUATION_OUTCOME_TRUE: &str = "true";
pub const POLICY_SIMULATION_EVALUATION_OUTCOME_FALSE: &str = "false";
pub const POLICY_SIMULATION_EVALUATION_OUTCOME_ERROR: &str = "error";

/// Port of `model.PolicySimulationEvaluationNode` (access_request.go:468) — one node of the
/// evaluation tree.
///
/// **Short-circuited branches are walked anyway**, so the consumer can render the state of every
/// clause rather than only the one that decided the verdict.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySimulationEvaluationNode {
    /// One of the `POLICY_SIMULATION_EVALUATION_KIND_*` constants.
    #[serde(rename = "kind")]
    pub kind: String,

    /// The textual form of **this subtree**, so the UI need not rebuild it from the AST.
    #[serde(rename = "expression")]
    pub expression: String,

    #[serde(rename = "outcome")]
    pub outcome: String,

    /// Populated only when `outcome` is `error`.
    #[serde(rename = "error", skip_serializing_if = "is_empty_str")]
    pub error: String,

    /// `==`, `!=`, `<`, `>`, `>=`, `<=`, `in`, `startsWith`, `endsWith`, `contains`. Empty on
    /// compound nodes.
    #[serde(rename = "operator", skip_serializing_if = "is_empty_str")]
    pub operator: String,

    /// The attribute path the leaf references, when unambiguous.
    #[serde(rename = "attribute", skip_serializing_if = "is_empty_str")]
    pub attribute: String,

    /// Display-formatted. **Empty means the attribute was missing**, which also shows as
    /// `outcome: error`.
    #[serde(rename = "actual_value", skip_serializing_if = "is_empty_str")]
    pub actual_value: String,

    /// Empty when the other side is itself an attribute reference.
    #[serde(rename = "expected_value", skip_serializing_if = "is_empty_str")]
    pub expected_value: String,

    #[serde(rename = "children", skip_serializing_if = "is_none_or_empty_vec")]
    pub children: Option<Vec<PolicySimulationEvaluationNode>>,
}

/// Port of `model.PolicySimulationMergedRule` (access_request.go:423) — one rule OR-folded into
/// a blame's merged expression.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySimulationMergedRule {
    #[serde(rename = "name")]
    pub name: String,

    /// The rule's CEL text **before** the OR-fold wrapped it in parentheses.
    #[serde(rename = "expression", skip_serializing_if = "is_empty_str")]
    pub expression: String,

    /// The standalone tree for this rule alone, so the UI can show "rule 1: TRUE / rule 2:
    /// FALSE" beside the merged tree.
    #[serde(rename = "evaluation_tree", skip_serializing_if = "is_none")]
    pub evaluation_tree: Option<Box<PolicySimulationEvaluationNode>>,
}

/// Port of `model.PolicySimulationBlame` (access_request.go:359).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySimulationBlame {
    /// One of the `POLICY_SIMULATION_BLAME_SOURCE_*` constants.
    #[serde(rename = "source")]
    pub source: String,

    /// Empty means `deny` — see the constants.
    #[serde(rename = "outcome", skip_serializing_if = "is_empty_str")]
    pub outcome: String,

    /// Empty when the deny came from the draft itself, which has no persisted id yet.
    #[serde(rename = "policy_id", skip_serializing_if = "is_empty_str")]
    pub policy_id: String,

    #[serde(rename = "policy_name", skip_serializing_if = "is_empty_str")]
    pub policy_name: String,

    #[serde(rename = "rule_name", skip_serializing_if = "is_empty_str")]
    pub rule_name: String,

    #[serde(rename = "role", skip_serializing_if = "is_empty_str")]
    pub role: String,

    /// **Scope-private**: populated only for same-scope blame. See the source constants.
    #[serde(rename = "expression", skip_serializing_if = "is_empty_str")]
    pub expression: String,

    /// Same scope-privacy rule as `expression`.
    #[serde(rename = "evaluation_tree", skip_serializing_if = "is_none")]
    pub evaluation_tree: Option<Box<PolicySimulationEvaluationNode>>,

    /// Populated only when more than one rule shares the contributing `(role, action)`; the
    /// order mirrors the policy's rule order, which is also the fold order.
    #[serde(rename = "merged_rules", skip_serializing_if = "is_none_or_empty_vec")]
    pub merged_rules: Option<Vec<PolicySimulationMergedRule>>,
}

/// Port of `model.PolicySimulationActionDecision` (access_request.go:505).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySimulationActionDecision {
    #[serde(rename = "decision")]
    pub decision: bool,

    #[serde(rename = "blame", skip_serializing_if = "is_none_or_empty_vec")]
    pub blame: Option<Vec<PolicySimulationBlame>>,
}

/// Port of `model.PolicySimulationSession` (access_request.go:517) — one session's breakdown.
///
/// A channel admin gets at most one **synthetic** session, with an empty `id`, which they may
/// override in the picker; a system admin gets their real sessions evaluated individually, which
/// is how the picker can explain two sessions of one user disagreeing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySimulationSession {
    /// Empty for a synthetic session.
    #[serde(rename = "id", skip_serializing_if = "is_empty_str")]
    pub id: String,

    #[serde(rename = "device", skip_serializing_if = "is_empty_str")]
    pub device: String,

    #[serde(rename = "network", skip_serializing_if = "is_empty_str")]
    pub network: String,

    /// Epoch milliseconds.
    #[serde(rename = "last_active_at", skip_serializing_if = "is_zero_i64")]
    pub last_active_at: i64,

    /// Action name → verdict, using **this session's** attributes; the profile attributes are
    /// constant across sessions.
    #[serde(rename = "decisions", skip_serializing_if = "is_none")]
    pub decisions: Option<std::collections::BTreeMap<String, PolicySimulationActionDecision>>,

    /// The session-attribute snapshot used. **`map[string]string` here**, while
    /// `Subject.Session` is `map[string]any` — the snapshot is already display-formatted.
    #[serde(rename = "attributes", skip_serializing_if = "is_none")]
    pub attributes: Option<StringMap>,
}

/// Port of `model.PolicySimulationUserResult` (access_request.go:539) — one row of the response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySimulationUserResult {
    #[serde(rename = "user")]
    pub user: Option<Box<User>>,

    /// The **headline** verdict when `sessions` is populated, so the picker can render one chip
    /// without consulting the per-session breakdown. Nil in expression-only fallback mode.
    #[serde(rename = "decisions", skip_serializing_if = "is_none")]
    pub decisions: Option<std::collections::BTreeMap<String, PolicySimulationActionDecision>>,

    #[serde(rename = "sessions", skip_serializing_if = "is_none_or_empty_vec")]
    pub sessions: Option<Vec<PolicySimulationSession>>,

    /// The profile-attribute snapshot used, display-formatted.
    #[serde(rename = "attributes", skip_serializing_if = "is_none")]
    pub attributes: Option<StringMap>,
}

/// Port of `model.PolicySimulationResponse` (access_request.go:559).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySimulationResponse {
    #[serde(rename = "results")]
    pub results: Option<Vec<PolicySimulationUserResult>>,

    #[serde(rename = "total")]
    pub total: i64,
}

/// Port of `model.PolicySimulationUserOverride` (access_request.go:571).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySimulationUserOverride {
    #[serde(rename = "user_id")]
    pub user_id: String,

    /// **Effectively a no-op**, kept for API compatibility: the simulator now always layers the
    /// requesting admin's resolved session snapshot underneath `session_overrides`. New clients
    /// leave it unset.
    #[serde(rename = "use_active_session", skip_serializing_if = "is_false")]
    pub use_active_session: bool,

    /// Overrides individual `session.*` attributes on top of that snapshot. Mirrors
    /// `Subject.Session`'s `map[string]any` so mixed types survive — a boolean `device_managed`
    /// beside a string `network_status` — without coercing everything to string.
    #[serde(
        rename = "session_overrides",
        skip_serializing_if = "is_none_or_empty_map"
    )]
    pub session_overrides: Option<StringInterface>,
}

/// Evaluate **only** the rule being edited: siblings, permission policies, imported parents and
/// peers are all excluded. The default when the request omits the scope.
pub const POLICY_EVALUATION_SCOPE_THIS_RULE: &str = "this_rule";
/// Co-evaluate every contributing program, exactly as the live PDP would.
pub const POLICY_EVALUATION_SCOPE_ALL: &str = "all";

/// Port of `model.PolicySimulationByUsersParams` (access_request.go:619) — the request body for
/// `/access_control_policies/cel/simulate_users`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicySimulationByUsersParams {
    /// The draft as it sits in the editor — **compiled in memory, never persisted**.
    #[serde(rename = "policy")]
    pub policy: Option<Box<crate::access_policy::AccessControlPolicy>>,

    /// Required: a picker only makes sense once an action is in scope.
    #[serde(rename = "actions")]
    pub actions: Option<Vec<String>>,

    /// Which rule the author is editing, for blame attribution. Denies from it are tagged
    /// `this_rule`; other denies in the same draft are `sibling_rule`.
    #[serde(rename = "rule_name", skip_serializing_if = "is_empty_str")]
    pub rule_name: String,

    #[serde(rename = "channel_id", skip_serializing_if = "is_empty_str")]
    pub channel_id: String,

    #[serde(rename = "team_id", skip_serializing_if = "is_empty_str")]
    pub team_id: String,

    #[serde(rename = "users")]
    pub users: Option<Vec<PolicySimulationUserOverride>>,

    /// Empty defaults to [`POLICY_EVALUATION_SCOPE_THIS_RULE`] **on the server**, not here.
    #[serde(rename = "evaluation_scope", skip_serializing_if = "is_empty_str")]
    pub evaluation_scope: String,
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
    fn scoped_role_round_trips_the_fixture() {
        assert_fixture_round_trips!(ScopedRole, "scoped_role");
    }
    #[test]
    fn subject_round_trips_the_fixture() {
        assert_fixture_round_trips!(Subject, "subject");
    }
    #[test]
    fn subject_search_options_round_trips_the_fixture() {
        assert_fixture_round_trips!(SubjectSearchOptions, "subject_search_options");
    }
    #[test]
    fn subject_cursor_round_trips_the_fixture() {
        assert_fixture_round_trips!(SubjectCursor, "subject_cursor");
    }
    #[test]
    fn resource_round_trips_the_fixture() {
        assert_fixture_round_trips!(Resource, "resource");
    }
    #[test]
    fn access_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(AccessRequest, "access_request");
    }
    #[test]
    fn access_decision_round_trips_the_fixture() {
        assert_fixture_round_trips!(AccessDecision, "access_decision");
    }
    #[test]
    fn query_expression_params_round_trips_the_fixture() {
        assert_fixture_round_trips!(QueryExpressionParams, "query_expression_params");
    }
    #[test]
    fn policy_simulation_blame_round_trips_the_fixture() {
        assert_fixture_round_trips!(PolicySimulationBlame, "policy_simulation_blame");
    }
    #[test]
    fn policy_simulation_merged_rule_round_trips_the_fixture() {
        assert_fixture_round_trips!(PolicySimulationMergedRule, "policy_simulation_merged_rule");
    }
    #[test]
    fn policy_simulation_evaluation_node_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            PolicySimulationEvaluationNode,
            "policy_simulation_evaluation_node"
        );
    }
    #[test]
    fn policy_simulation_action_decision_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            PolicySimulationActionDecision,
            "policy_simulation_action_decision"
        );
    }
    #[test]
    fn policy_simulation_session_round_trips_the_fixture() {
        assert_fixture_round_trips!(PolicySimulationSession, "policy_simulation_session");
    }
    #[test]
    fn policy_simulation_user_result_round_trips_the_fixture() {
        assert_fixture_round_trips!(PolicySimulationUserResult, "policy_simulation_user_result");
    }
    #[test]
    fn policy_simulation_response_round_trips_the_fixture() {
        assert_fixture_round_trips!(PolicySimulationResponse, "policy_simulation_response");
    }
    #[test]
    fn policy_simulation_user_override_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            PolicySimulationUserOverride,
            "policy_simulation_user_override"
        );
    }
    #[test]
    fn policy_simulation_by_users_params_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            PolicySimulationByUsersParams,
            "policy_simulation_by_users_params"
        );
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// The scoped-role accessors are a two-source lookup with a deprecated fallback: the system
    /// scope falls back to `role` when `scoped_roles` has no system entry, the channel scope has
    /// no fallback at all, and `SetScopedRole("", …)` is a no-op rather than an insert.
    ///
    /// `duplicate_scopes` pins which of two entries for one scope wins, and `set(channel,"")`
    /// pins that an empty role **removes** the entry.
    #[test]
    fn scoped_roles_match_go() {
        let oracle = oracle();
        let cases = oracle["subject_scoped_roles"].as_array().unwrap();

        /// One corpus row's subject, rebuilt fresh for the read case and again for each write.
        type Builder = fn() -> Subject;
        let builders: Vec<(&str, Builder)> = vec![
            ("legacy_only", || Subject {
                role: "system_admin".to_string(),
                ..Default::default()
            }),
            ("channel_only", || Subject {
                role: "system_admin".to_string(),
                scoped_roles: Some(vec![ScopedRole {
                    scope: ACCESS_CONTROL_SUBJECT_SCOPE_CHANNEL.to_string(),
                    role: "channel_admin".to_string(),
                }]),
                ..Default::default()
            }),
            ("both", || Subject {
                role: "system_admin".to_string(),
                scoped_roles: Some(vec![
                    ScopedRole {
                        scope: ACCESS_CONTROL_SUBJECT_SCOPE_SYSTEM.to_string(),
                        role: "system_user".to_string(),
                    },
                    ScopedRole {
                        scope: ACCESS_CONTROL_SUBJECT_SCOPE_CHANNEL.to_string(),
                        role: "channel_user".to_string(),
                    },
                ]),
                ..Default::default()
            }),
            ("duplicate_scopes", || Subject {
                scoped_roles: Some(vec![
                    ScopedRole {
                        scope: ACCESS_CONTROL_SUBJECT_SCOPE_CHANNEL.to_string(),
                        role: "channel_user".to_string(),
                    },
                    ScopedRole {
                        scope: ACCESS_CONTROL_SUBJECT_SCOPE_CHANNEL.to_string(),
                        role: "channel_admin".to_string(),
                    },
                ]),
                ..Default::default()
            }),
        ];
        let sets = [
            (ACCESS_CONTROL_SUBJECT_SCOPE_CHANNEL, "channel_guest"),
            (ACCESS_CONTROL_SUBJECT_SCOPE_CHANNEL, ""),
            ("", "ignored"),
            (ACCESS_CONTROL_SUBJECT_SCOPE_SYSTEM, "system_guest"),
        ];
        assert_eq!(cases.len(), builders.len() * (1 + sets.len()));

        let mut case = cases.iter();
        for (label, build) in builders {
            let read = case.next().unwrap();
            assert_eq!(
                read["name"].as_str().unwrap(),
                format!("{label}:read"),
                "corpus order drifted"
            );
            let s = build();
            assert_eq!(
                s.role_for_scope(ACCESS_CONTROL_SUBJECT_SCOPE_SYSTEM),
                read["system_role"].as_str().unwrap(),
                "{label} system"
            );
            assert_eq!(
                s.role_for_scope(ACCESS_CONTROL_SUBJECT_SCOPE_CHANNEL),
                read["channel_role"].as_str().unwrap(),
                "{label} channel"
            );
            assert_eq!(
                s.role_for_scope("team"),
                read["unknown_scope"].as_str().unwrap(),
                "{label} unknown scope"
            );
            for (scope, key) in [
                (ACCESS_CONTROL_SUBJECT_SCOPE_SYSTEM, "system_roles"),
                (ACCESS_CONTROL_SUBJECT_SCOPE_CHANNEL, "channel_roles"),
            ] {
                // Go's nil slice reaches the corpus as `null`; the port answers with an empty
                // `Vec`, which is the same "no roles".
                let expected: Vec<String> =
                    serde_json::from_value(read[key].clone()).unwrap_or_default();
                assert_eq!(s.roles_for_scope(scope), expected, "{label} {key}");
            }

            for (scope, role) in sets {
                let written = case.next().unwrap();
                assert_eq!(
                    written["name"].as_str().unwrap(),
                    format!("{label}:set({scope},{role})"),
                    "corpus order drifted"
                );
                let mut s = build();
                s.set_scoped_role(scope, role);
                let expected: Vec<ScopedRole> =
                    serde_json::from_value(written["scoped_roles"].clone()).unwrap_or_default();
                assert_eq!(
                    s.scoped_roles.clone().unwrap_or_default(),
                    expected,
                    "{label} after set({scope},{role})"
                );
                assert_eq!(
                    s.role_for_scope(ACCESS_CONTROL_SUBJECT_SCOPE_SYSTEM),
                    written["system_role"].as_str().unwrap(),
                    "{label} after set({scope},{role})"
                );
                assert_eq!(
                    s.role_for_scope(ACCESS_CONTROL_SUBJECT_SCOPE_CHANNEL),
                    written["channel_role"].as_str().unwrap(),
                    "{label} after set({scope},{role})"
                );
            }
        }
    }
}
