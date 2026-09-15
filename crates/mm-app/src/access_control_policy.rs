//! The app functions behind `api4/access_control.go` — ports of `app/access_control.go` and
//! `app/team_access_control.go` for a build whose access-control service is **nil**.
//!
//! `ch.AccessControl` is assigned only when the enterprise package registers
//! `AccessControlServiceInterface` (app/channels.go:203), which nothing in this tree does
//! ([D-571]). Every function that begins with `acs := a.Srv().ch.AccessControl; if acs == nil`
//! is therefore a constant: the 501 named after it, with the detail "Policy Administration
//! Point is not initialized". Those are ported as exactly that — the `where` and the id are
//! per function and a reader who swaps two of them changes the wire — and nothing past the nil
//! check is ported, because nothing past it can run.
//!
//! What *does* run on this build, and is ported for real, is everything that reads the store or
//! the permission tables **before** asking the service:
//!
//! - `ValidateTeamAdminPolicyOwnership` — two `SearchPolicies` calls, which decide a team
//!   admin's 403 against the service's 501 on the read, delete, activate and resource routes;
//! - `ReconcilePolicyTeamScope` — a search, a get and a save, run on every `assign`/`unassign`
//!   that carries no resource ids and so never reaches a nil check (a 200 on both servers);
//! - `ValidateTeamScopePolicyChannelAssignment`, `ValidateChannelEligibilityForAccessControl`,
//!   `ValidateChannelAccessControlPermission` and `ValidatePolicySimulationUsersInScope` —
//!   channel and membership reads whose 400/403/404 come before the service's 501;
//! - `GetAccessControlFieldsAutocomplete`, which never consults the service at all: the
//!   property group, a field search through the property hooks, and the four native attribute
//!   descriptors on the first page.

use std::collections::HashMap;

use mm_model::access_policy::{
    ACCESS_CONTROL_POLICY_SCOPE_TEAM, ACCESS_CONTROL_POLICY_TYPE_CHANNEL,
    ACCESS_CONTROL_POLICY_TYPE_PARENT, AccessControlPolicy, AccessControlPolicySearch,
};
use mm_model::access_request::PolicySimulationUserOverride;
use mm_model::channel::{CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE, Channel};
use mm_model::native_attributes::native_user_attribute_fields;
use mm_model::permission::{PERMISSION_MANAGE_CHANNEL_ACCESS_RULES, PERMISSION_MANAGE_SYSTEM};
use mm_model::property_field::{
    PROPERTY_FIELD_OBJECT_TYPE_USER, PropertyField, PropertyFieldSearchCursor,
    PropertyFieldSearchOpts,
};
use mm_model::utils::{AppError, AppResult, is_valid_id};
use mm_store::access_control_policy_store::AccessControlPolicyStore;
use mm_store::channel_store::ChannelStore;

use crate::App;
use crate::property_hooks::PropertyCaller;

/// `model.AccessControlPropertyGroupName` (property_group.go:11).
const ACCESS_CONTROL_PROPERTY_GROUP_NAME: &str = "access_control";

/// The detail every nil-service refusal carries. Wiped before the wire like every detail, but
/// it is what Go writes and what a log would show.
const NOT_INITIALIZED: &str = "Policy Administration Point is not initialized";

/// `acs == nil` — the 501 each function names after itself.
fn service_unavailable(where_: &'static str, id: &'static str) -> Box<AppError> {
    AppError::boxed(where_, id, None, NOT_INITIALIZED, 501)
}

impl App {
    /// Port of `App.GetAccessControlPolicy` (app/access_control.go:67): the nil-service 501
    /// `app.pap.get_policy.app_error`, whose `where` is **`GetPolicy`**.
    ///
    /// Note that this is *not* a 404: a caller that branches on "not found" — the delegated
    /// permission check, `preserveSystemManagedFields` — never takes that branch on this build.
    pub fn get_access_control_policy(&self, _id: &str) -> AppResult<AccessControlPolicy> {
        Err(service_unavailable(
            "GetPolicy",
            "app.pap.get_policy.app_error",
        ))
    }

    /// Port of `App.CreateOrUpdateAccessControlPolicy` (:81): the nil check is its first line,
    /// before the id is minted or the channel is validated.
    pub fn create_or_update_access_control_policy(
        &self,
        _policy: &AccessControlPolicy,
    ) -> AppResult<AccessControlPolicy> {
        Err(service_unavailable(
            "CreateAccessControlPolicy",
            "app.pap.create_access_control_policy.app_error",
        ))
    }

    /// Port of `App.DeleteAccessControlPolicy` (:435).
    pub fn delete_access_control_policy(&self, _id: &str) -> AppResult {
        Err(service_unavailable(
            "DeleteAccessControlPolicy",
            "app.pap.delete_access_control_policy.app_error",
        ))
    }

    /// Port of `App.CheckExpression` (:492).
    pub fn check_expression(&self, _expression: &str) -> AppResult {
        Err(service_unavailable(
            "CheckExpression",
            "app.pap.check_expression.app_error",
        ))
    }

    /// Port of `App.TestExpression` (:506) — the same id as `CheckExpression`, a different
    /// `where`.
    pub fn test_expression(&self, _expression: &str) -> AppResult {
        Err(service_unavailable(
            "TestExpression",
            "app.pap.check_expression.app_error",
        ))
    }

    /// Port of `App.ValidateExpressionAgainstRequester` (:2403). Both delegated-context test
    /// variants (`TestExpressionWithChannelContext`, `…WithTeamContext`) call this first, so
    /// their answer on this build is this one.
    pub fn validate_expression_against_requester(
        &self,
        _expression: &str,
        _requester_id: &str,
    ) -> AppResult<bool> {
        Err(service_unavailable(
            "ValidateExpressionAgainstRequester",
            "app.pap.check_expression.app_error",
        ))
    }

    /// Port of `App.SimulateAccessControlPolicyForUsers` (:550): the nil check precedes the
    /// masking merge and the session requirement.
    pub fn simulate_access_control_policy_for_users(&self) -> AppResult {
        Err(service_unavailable(
            "SimulateAccessControlPolicyForUsers",
            "app.pap.simulate.unavailable",
        ))
    }

    /// Port of `App.SearchAccessControlPolicies` (:1659). `SearchTeamAccessPolicies`
    /// (team_access_control.go:24) calls it before anything else, so the team-scoped search
    /// is this too.
    pub fn search_access_control_policies(&self) -> AppResult {
        Err(service_unavailable(
            "SearchAccessControlPolicies",
            "app.pap.search_access_control_policies.app_error",
        ))
    }

    /// Port of `App.UpdateAccessControlPoliciesActive` (:1758).
    pub fn update_access_control_policies_active(&self) -> AppResult {
        Err(service_unavailable(
            "UpdateAccessControlPoliciesActive",
            "app.pap.update_access_control_policies_active.app_error",
        ))
    }

    /// Port of `App.ExpressionToVisualAST` (:1808). `GetMaskedVisualAST`
    /// (access_control_masking.go:24) calls it first, so the masked variant is this too.
    pub fn expression_to_visual_ast(&self, _expression: &str) -> AppResult {
        Err(service_unavailable(
            "ExpressionToVisualAST",
            "app.pap.expression_to_visual_ast.app_error",
        ))
    }

    /// Port of `App.AssignAccessControlPolicyToChannels` (:1438).
    pub fn assign_access_control_policy_to_channels(&self) -> AppResult {
        Err(service_unavailable(
            "AssignAccessControlPolicyToChannels",
            "app.pap.assign_access_control_policy_to_channels.app_error",
        ))
    }

    /// Port of `App.AssignAccessControlPolicyToTeams` (:1548).
    pub fn assign_access_control_policy_to_teams(&self) -> AppResult {
        Err(service_unavailable(
            "AssignAccessControlPolicyToTeams",
            "app.pap.assign_access_control_policy_to_teams.app_error",
        ))
    }

    /// Port of `App.UnassignPoliciesFromChannels` (:1495).
    pub fn unassign_policies_from_channels(&self) -> AppResult {
        Err(service_unavailable(
            "UnassignPoliciesFromChannels",
            "app.pap.unassign_access_control_policy_from_channels.app_error",
        ))
    }

    /// Port of `App.UnassignPoliciesFromTeams` (:1605).
    pub fn unassign_policies_from_teams(&self) -> AppResult {
        Err(service_unavailable(
            "UnassignPoliciesFromTeams",
            "app.pap.unassign_access_control_policy_from_teams.app_error",
        ))
    }

    /// Port of `App.GetChannelsForPolicy` (:24): `GetAccessControlPolicy` first, so the
    /// `get_policy` 501 — not one of this function's own errors.
    pub fn get_channels_for_policy(&self, policy_id: &str) -> AppResult {
        self.get_access_control_policy(policy_id).map(|_| ())
    }

    /// Port of `App.ValidateChannelEligibilityForAccessControl` (:2177), in Go's order: the
    /// type, group constraint, shared, then a team default channel name.
    pub fn validate_channel_eligibility_for_access_control(&self, channel: &Channel) -> AppResult {
        if channel.channel_type != CHANNEL_TYPE_PRIVATE && channel.channel_type != CHANNEL_TYPE_OPEN
        {
            return Err(AppError::boxed(
                "ValidateChannelEligibilityForAccessControl",
                "app.pap.access_control.channel_type_not_supported",
                None,
                "Policies can only be applied to public or private channels",
                400,
            ));
        }
        if channel.is_group_constrained() {
            return Err(AppError::boxed(
                "ValidateChannelEligibilityForAccessControl",
                "app.pap.access_control.channel_group_constrained",
                None,
                "Channel is group constrained",
                400,
            ));
        }
        if channel.is_shared() {
            return Err(AppError::boxed(
                "ValidateChannelEligibilityForAccessControl",
                "app.pap.access_control.channel_shared",
                None,
                "Channel is shared",
                400,
            ));
        }
        if self.default_channel_names().contains(&channel.name) {
            return Err(AppError::boxed(
                "ValidateChannelEligibilityForAccessControl",
                "app.pap.access_control.channel_default",
                None,
                "Channel is a team default channel",
                400,
            ));
        }
        Ok(())
    }

    /// Port of `App.ValidateChannelAccessControlPermission` (:2219): the channel must exist,
    /// the user must hold `manage_channel_access_rules` in it, and it must be eligible.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id))]
    pub async fn validate_channel_access_control_permission(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> AppResult {
        let channel = self.get_channel(channel_id).await?;
        let (ok, _) = self
            .has_permission_to_channel(user_id, channel_id, &PERMISSION_MANAGE_CHANNEL_ACCESS_RULES)
            .await;
        if !ok {
            return Err(AppError::boxed(
                "ValidateChannelAccessControlPermission",
                "app.pap.access_control.insufficient_channel_permissions",
                None,
                format!("user_id={user_id} channel_id={channel_id}"),
                403,
            ));
        }
        self.validate_channel_eligibility_for_access_control(&channel)
    }

    /// Port of `App.ValidateAccessControlPolicyPermissionWithOptions` (:2248).
    ///
    /// A system admin (by the user's roles, not the session's) passes. Everyone else needs the
    /// policy, and on this build the policy read is the `get_policy` **501**, which is not the
    /// 404 the channel-permission fallback keys on — so the 501 is what comes back, and every
    /// caller treats a non-nil error as "not permitted". The read-only channel-context arm and
    /// the channel-type arm are ported for the day the read answers.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, policy_id = %policy_id))]
    pub async fn validate_access_control_policy_permission_with_options(
        &self,
        user_id: &str,
        policy_id: &str,
        is_read_only: bool,
        channel_id: &str,
    ) -> AppResult {
        if self
            .has_permission_to(user_id, &PERMISSION_MANAGE_SYSTEM)
            .await
        {
            return Ok(());
        }

        let policy = match self.get_access_control_policy(policy_id) {
            Ok(policy) => policy,
            Err(err) => {
                if err.status_code == 404
                    && self
                        .validate_channel_access_control_permission(user_id, policy_id)
                        .await
                        .is_ok()
                {
                    return Ok(());
                }
                return Err(err);
            }
        };

        if is_read_only
            && policy.type_ != ACCESS_CONTROL_POLICY_TYPE_CHANNEL
            && !channel_id.is_empty()
        {
            let (ok, _) = self
                .has_permission_to_channel(
                    user_id,
                    channel_id,
                    &mm_model::permission::PERMISSION_READ_CHANNEL,
                )
                .await;
            if !ok {
                return Err(AppError::boxed(
                    "ValidateAccessControlPolicyPermissionWithOptions",
                    "app.pap.access_control.insufficient_permissions",
                    None,
                    format!("user_id={user_id} channel_id={channel_id}"),
                    403,
                ));
            }
            // `isSystemPolicyAppliedToChannel`: the channel's own policy, which is the 501 here.
            if let Ok(channel_policy) = self.get_access_control_policy(channel_id)
                && channel_policy
                    .imports
                    .as_deref()
                    .is_some_and(|imports| imports.iter().any(|import| import == policy_id))
            {
                return Ok(());
            }
            return Err(AppError::boxed(
                "ValidateAccessControlPolicyPermissionWithOptions",
                "app.pap.access_control.insufficient_permissions",
                None,
                format!(
                    "user_id={user_id} policy_type={} channel_id={channel_id}",
                    policy.type_
                ),
                403,
            ));
        }

        if policy.type_ != ACCESS_CONTROL_POLICY_TYPE_CHANNEL {
            return Err(AppError::boxed(
                "ValidateAccessControlPolicyPermissionWithOptions",
                "app.pap.access_control.insufficient_permissions",
                None,
                format!("user_id={user_id} policy_type={}", policy.type_),
                403,
            ));
        }
        self.validate_channel_access_control_permission(user_id, policy_id)
            .await
    }

    /// Port of `App.ValidateAccessControlPolicyPermission` (:2239) — no read-only mode, no
    /// channel context.
    pub async fn validate_access_control_policy_permission(
        &self,
        user_id: &str,
        policy_id: &str,
    ) -> AppResult {
        self.validate_access_control_policy_permission_with_options(user_id, policy_id, false, "")
            .await
    }

    /// Port of `App.ValidateChannelAccessControlPolicyCreation` (:2327).
    #[tracing::instrument(skip(self, policy), fields(user_id = %user_id, policy_id = %policy.id))]
    pub async fn validate_channel_access_control_policy_creation(
        &self,
        user_id: &str,
        policy: &AccessControlPolicy,
    ) -> AppResult {
        if self
            .has_permission_to(user_id, &PERMISSION_MANAGE_SYSTEM)
            .await
        {
            return Ok(());
        }
        if policy.type_ != ACCESS_CONTROL_POLICY_TYPE_CHANNEL {
            return Err(AppError::boxed(
                "ValidateChannelAccessControlPolicyCreation",
                "app.access_control.insufficient_permissions",
                None,
                format!("user_id={user_id} policy_type={}", policy.type_),
                403,
            ));
        }
        self.validate_channel_access_control_permission(user_id, &policy.id)
            .await
    }

    /// Port of `App.ValidateTeamAdminSelfInclusion` (team_access_control.go:332): an empty
    /// expression passes; anything else asks the service, and its 501 comes back wrapped as
    /// this function's **500** `app.team.access_policies.validation_error.app_error`. The
    /// `self_exclusion` 400 is unreachable on this build.
    pub fn validate_team_admin_self_inclusion(&self, user_id: &str, expression: &str) -> AppResult {
        if expression.is_empty() {
            return Ok(());
        }
        match self.validate_expression_against_requester(expression, user_id) {
            Ok(true) => Ok(()),
            Ok(false) => Err(AppError::boxed(
                "ValidateTeamAdminSelfInclusion",
                "app.team.access_policies.self_exclusion.app_error",
                None,
                "policy rules would exclude the requesting admin",
                400,
            )),
            Err(err) => Err(Box::new(
                AppError::new(
                    "ValidateTeamAdminSelfInclusion",
                    "app.team.access_policies.validation_error.app_error",
                    None,
                    String::new(),
                    500,
                )
                .wrap(*err),
            )),
        }
    }

    /// Port of `App.ValidateTeamAdminPolicyOwnership` (team_access_control.go:164): explicit
    /// scope first (`scope=team`, `scope_id=<team>`), then channel inference (every child
    /// channel in the team). Both are store searches with `Type = parent`, `IDs = [policy]`,
    /// `Limit = 1`; either store failure is the 500 `ownership_check`.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, policy_id = %policy_id, owned))]
    pub async fn validate_team_admin_policy_ownership(
        &self,
        team_id: &str,
        policy_id: &str,
    ) -> AppResult<bool> {
        let ownership_failure = |err: mm_store::error::StoreError| {
            tracing::error!(error = %err, "the policy ownership search failed");
            Box::new(
                AppError::new(
                    "ValidateTeamAdminPolicyOwnership",
                    "app.team.access_policies.ownership_check.app_error",
                    None,
                    String::new(),
                    500,
                )
                .wrap(err),
            )
        };

        let (scoped, _) = self
            .store()
            .access_control_policy()
            .search_policies(&AccessControlPolicySearch {
                ids: Some(vec![policy_id.to_owned()]),
                type_: ACCESS_CONTROL_POLICY_TYPE_PARENT.to_owned(),
                scope: ACCESS_CONTROL_POLICY_SCOPE_TEAM.to_owned(),
                scope_id: team_id.to_owned(),
                limit: 1,
                ..AccessControlPolicySearch::default()
            })
            .await
            .map_err(ownership_failure)?;
        if !scoped.is_empty() {
            tracing::Span::current().record("owned", true);
            return Ok(true);
        }

        let (inferred, _) = self
            .store()
            .access_control_policy()
            .search_policies(&AccessControlPolicySearch {
                ids: Some(vec![policy_id.to_owned()]),
                type_: ACCESS_CONTROL_POLICY_TYPE_PARENT.to_owned(),
                team_id: team_id.to_owned(),
                limit: 1,
                ..AccessControlPolicySearch::default()
            })
            .await
            .map_err(ownership_failure)?;
        let owned = !inferred.is_empty();
        tracing::Span::current().record("owned", owned);
        Ok(owned)
    }

    /// Port of `App.ReconcilePolicyTeamScope` (team_access_control.go:206).
    ///
    /// The child channel policies (`Type = channel`, importing this one, up to 1000) name the
    /// channels; no children means the scope is left alone, and a channel that no longer
    /// resolves (`GetChannels` returns fewer rows) skips the reconcile rather than stamping a
    /// stale team. One team → `scope=team` on that team; several → the scope is cleared. The
    /// parent is read and written **straight through the store**, not the service — which is
    /// why this runs on a build where the service is nil — and only when something changed.
    #[tracing::instrument(skip(self), fields(policy_id = %policy_id, children, outcome))]
    pub async fn reconcile_policy_team_scope(&self, policy_id: &str) -> AppResult {
        let reconcile_failure = |err: mm_store::error::StoreError| {
            tracing::error!(error = %err, "the team-scope reconcile failed");
            Box::new(
                AppError::new(
                    "ReconcilePolicyTeamScope",
                    "app.team.access_policies.reconcile_scope.app_error",
                    None,
                    String::new(),
                    500,
                )
                .wrap(err),
            )
        };

        let (children, _) = self
            .store()
            .access_control_policy()
            .search_policies(&AccessControlPolicySearch {
                parent_id: policy_id.to_owned(),
                type_: ACCESS_CONTROL_POLICY_TYPE_CHANNEL.to_owned(),
                limit: 1000,
                ..AccessControlPolicySearch::default()
            })
            .await
            .map_err(reconcile_failure)?;
        tracing::Span::current().record("children", children.len());
        if children.is_empty() {
            tracing::Span::current().record("outcome", "no children");
            return Ok(());
        }

        let channel_ids: Vec<String> = children.into_iter().map(|child| child.id).collect();
        let channels = self.get_channels(&channel_ids).await?;
        if channels.len() != channel_ids.len() {
            tracing::Span::current().record("outcome", "a child channel is gone");
            return Ok(());
        }

        let mut team_ids: Vec<&str> = channels.iter().map(|ch| ch.team_id.as_str()).collect();
        team_ids.sort_unstable();
        team_ids.dedup();

        let mut policy = self
            .store()
            .access_control_policy()
            .get(policy_id)
            .await
            .map_err(reconcile_failure)?;

        let (new_scope, new_scope_id) = match team_ids.as_slice() {
            [team_id] => (ACCESS_CONTROL_POLICY_SCOPE_TEAM, *team_id),
            _ => ("", ""),
        };
        if policy.scope == new_scope && policy.scope_id == new_scope_id {
            tracing::Span::current().record("outcome", "unchanged");
            return Ok(());
        }

        policy.scope = new_scope.to_owned();
        policy.scope_id = new_scope_id.to_owned();
        self.store()
            .access_control_policy()
            .save(&policy)
            .await
            .map_err(reconcile_failure)?;
        tracing::Span::current().record("outcome", "saved");
        Ok(())
    }

    /// Port of `App.ValidateTeamScopePolicyChannelAssignment` (team_access_control.go:296): at
    /// least one channel, all of them found, all in the team, all eligible — in that order,
    /// each with its own 400.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, asked = channel_ids.len()))]
    pub async fn validate_team_scope_policy_channel_assignment(
        &self,
        team_id: &str,
        channel_ids: &[String],
    ) -> AppResult {
        if channel_ids.is_empty() {
            return Err(AppError::boxed(
                "ValidateTeamScopePolicyChannelAssignment",
                "app.team.access_policies.channels_required.app_error",
                None,
                "at least one channel is required",
                400,
            ));
        }
        let channels = self.get_channels(channel_ids).await?;
        if channels.len() != channel_ids.len() {
            return Err(AppError::boxed(
                "ValidateTeamScopePolicyChannelAssignment",
                "app.team.access_policies.channel_not_found.app_error",
                None,
                "one or more channels not found",
                400,
            ));
        }
        for channel in &channels {
            if channel.team_id != team_id {
                let params = HashMap::from([(
                    "ChannelId".to_owned(),
                    serde_json::Value::String(channel.id.clone()),
                )]);
                return Err(AppError::boxed(
                    "ValidateTeamScopePolicyChannelAssignment",
                    "app.team.access_policies.channel_wrong_team.app_error",
                    Some(params),
                    "channel does not belong to this team",
                    400,
                ));
            }
            self.validate_channel_eligibility_for_access_control(channel)?;
        }
        Ok(())
    }

    /// Port of `App.ValidatePolicySimulationUsersInScope` (app/access_control.go:632): with a
    /// channel, every user must be a channel member; otherwise, with a team, a team member. A
    /// missing membership is the 403 `users_out_of_scope`; a malformed id a 400; and the
    /// channel arm's lookup goes to the store directly, so its other failures are the 500
    /// `app.channel.get_member.app_error`.
    #[tracing::instrument(skip(self, users), fields(team_id = %team_id, channel_id = %channel_id, users = users.len()))]
    pub async fn validate_policy_simulation_users_in_scope(
        &self,
        team_id: &str,
        channel_id: &str,
        users: &[PolicySimulationUserOverride],
    ) -> AppResult {
        let invalid = |name: &str| {
            let params = HashMap::from([(
                "Name".to_owned(),
                serde_json::Value::String(name.to_owned()),
            )]);
            AppError::boxed(
                "ValidatePolicySimulationUsersInScope",
                "api.context.invalid_param.app_error",
                Some(params),
                String::new(),
                400,
            )
        };
        let out_of_scope = |user_id: &str| {
            AppError::boxed(
                "ValidatePolicySimulationUsersInScope",
                "api.access_control_policy.simulate.users_out_of_scope.app_error",
                None,
                format!("user_id={user_id}"),
                403,
            )
        };

        if !channel_id.is_empty() {
            if !is_valid_id(channel_id) {
                return Err(invalid("channel_id"));
            }
            for user in users {
                if user.user_id.is_empty() || !is_valid_id(&user.user_id) {
                    return Err(invalid("user_id"));
                }
                if let Err(err) = self
                    .store()
                    .channel()
                    .get_member(channel_id, &user.user_id)
                    .await
                {
                    if err.is_not_found() {
                        return Err(out_of_scope(&user.user_id));
                    }
                    tracing::error!(error = %err, "channel member lookup failed");
                    return Err(Box::new(
                        AppError::new(
                            "ValidatePolicySimulationUsersInScope",
                            "app.channel.get_member.app_error",
                            None,
                            String::new(),
                            500,
                        )
                        .wrap(err),
                    ));
                }
            }
            return Ok(());
        }
        if !team_id.is_empty() {
            if !is_valid_id(team_id) {
                return Err(invalid("team_id"));
            }
            for user in users {
                if user.user_id.is_empty() || !is_valid_id(&user.user_id) {
                    return Err(invalid("user_id"));
                }
                if let Err(err) = self.get_team_member(team_id, &user.user_id).await {
                    if err.status_code == 404 {
                        return Err(out_of_scope(&user.user_id));
                    }
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    /// Port of `App.GetAccessControlFieldsAutocomplete` (app/access_control.go:1727) — the one
    /// function in this family that never asks the service.
    ///
    /// The `access_control` property group, then a `user`-object field search whose cursor is
    /// `(CreateAt: 1, PropertyFieldID: after)` — so `after` only orders among fields created at
    /// millisecond 1, i.e. it is effectively ignored for every real row — through the property
    /// hooks with the caller's **raw user id** (`RequestContextWithCallerID`, not the
    /// `sessionCallerID` mapping the properties API uses, so a local-mode caller is `""` here).
    /// Any failure past the group lookup is the 500 `get_access_control_auto_complete`,
    /// including the licence hook's 403 on an unlicensed server with at least one field. The
    /// four native attribute descriptors are prepended on the first page, which is `after` empty
    /// **or** the 26-zero sentinel the handler substitutes.
    #[tracing::instrument(skip(self), fields(after = %after, limit, caller_id = %caller_id, found))]
    pub async fn get_access_control_fields_autocomplete(
        &self,
        after: &str,
        limit: i64,
        caller_id: &str,
    ) -> AppResult<Vec<PropertyField>> {
        let group = match self
            .get_property_group(ACCESS_CONTROL_PROPERTY_GROUP_NAME)
            .await
        {
            Ok(group) => group,
            Err(err) => {
                return Err(Box::new(
                    AppError::new(
                        "GetAccessControlAutoComplete",
                        "app.pap.get_access_control_auto_complete.app_error",
                        None,
                        String::new(),
                        500,
                    )
                    .wrap(*err),
                ));
            }
        };

        let caller = PropertyCaller {
            id: caller_id.to_owned(),
            acting_as_scope: String::new(),
        };
        let opts = PropertyFieldSearchOpts {
            group_id: group.id.clone(),
            object_type: PROPERTY_FIELD_OBJECT_TYPE_USER.to_owned(),
            cursor: PropertyFieldSearchCursor {
                property_field_id: after.to_owned(),
                create_at: 1,
                update_at: 0,
            },
            per_page: limit,
            ..PropertyFieldSearchOpts::default()
        };
        let mut fields = match self.search_property_fields(&group, &opts, &caller).await {
            Ok(fields) => fields,
            Err(err) => {
                return Err(AppError::boxed(
                    "GetAccessControlAutoComplete",
                    "app.pap.get_access_control_auto_complete.app_error",
                    None,
                    err.to_string(),
                    500,
                ));
            }
        };
        tracing::Span::current().record("found", fields.len());

        if after.is_empty() || after == "0".repeat(26) {
            let mut page = native_user_attribute_fields(&group.id);
            page.append(&mut fields);
            fields = page;
        }
        Ok(fields)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every nil-service refusal is a 501 whose `where` and id are the function's own; the two
    /// that share an id (`CheckExpression`/`TestExpression`) differ in `where`.
    #[test]
    fn the_nil_service_refusals_carry_their_own_names() {
        let cases: [(Box<AppError>, &str, &str); 6] = [
            (
                service_unavailable("GetPolicy", "app.pap.get_policy.app_error"),
                "GetPolicy",
                "app.pap.get_policy.app_error",
            ),
            (
                service_unavailable("CheckExpression", "app.pap.check_expression.app_error"),
                "CheckExpression",
                "app.pap.check_expression.app_error",
            ),
            (
                service_unavailable("TestExpression", "app.pap.check_expression.app_error"),
                "TestExpression",
                "app.pap.check_expression.app_error",
            ),
            (
                service_unavailable(
                    "SimulateAccessControlPolicyForUsers",
                    "app.pap.simulate.unavailable",
                ),
                "SimulateAccessControlPolicyForUsers",
                "app.pap.simulate.unavailable",
            ),
            (
                service_unavailable(
                    "UnassignPoliciesFromTeams",
                    "app.pap.unassign_access_control_policy_from_teams.app_error",
                ),
                "UnassignPoliciesFromTeams",
                "app.pap.unassign_access_control_policy_from_teams.app_error",
            ),
            (
                service_unavailable(
                    "ExpressionToVisualAST",
                    "app.pap.expression_to_visual_ast.app_error",
                ),
                "ExpressionToVisualAST",
                "app.pap.expression_to_visual_ast.app_error",
            ),
        ];
        for (err, where_, id) in cases {
            assert_eq!(err.status_code, 501);
            assert_eq!(err.where_, where_);
            assert_eq!(err.id, id);
            assert_eq!(err.detailed_error, NOT_INITIALIZED);
        }
    }
}
