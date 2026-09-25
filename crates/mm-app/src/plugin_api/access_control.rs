//! The plugin API's access-control methods (app/plugin_access_control.go), as Go's public build
//! answers them — docs/PLUGIN_PLAN.md, Phase 6.
//!
//! Go's policy engine (`Srv().ch.AccessControl`) is enterprise code the reference tree does not
//! hold, so on every build this project can run it is nil. What remains is public and ported
//! whole: each method's own checks in Go's order — the resource type's format and owner, the
//! action, the ids, the acting user — and then what a nil engine answers. That is a 501 for the
//! PAP methods; for `EvaluateAccessControl` it is `resolvePluginPolicyExistence`'s raw store
//! read — the vacuous `no_policy` allow when no policy of the requested type exists under the
//! resource id, and 503 when one does (it could not be evaluated, so the caller must deny).
//!
//! The two audited methods (`SaveAccessControlPolicy`, `DeleteAccessControlPolicy`) write their
//! record to the audit log, which this server does not keep ([D-1330]).

use mm_model::access_policy::{
    is_plugin_access_control_policy_type, is_valid_policy_action,
    plugin_owns_access_control_policy_type,
};
use mm_model::access_request::AccessDecision;
use mm_model::utils::{AppError, is_valid_id};
use mm_plugin::rpc::NotImplemented;
use mm_plugin::wire::model as wire;
use mm_plugin::wire::plugin as api;
use mm_store::{AccessControlPolicyStore, StoreError};

use super::AppPluginApi;
use crate::plugin_hooks::props_to_wire;

/// "Policy Administration Point is not initialized", the 501 of a nil engine.
fn pap_not_initialized(where_: &'static str, id: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        id,
        None,
        "Policy Administration Point is not initialized",
        501,
    )
}

fn invalid_id(where_: &'static str, detail: &str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "app.access_control.plugin.invalid_id.app_error",
        None,
        detail,
        400,
    )
}

fn decision_to_wire(decision: &AccessDecision) -> wire::AccessDecision {
    wire::AccessDecision {
        decision: decision.decision,
        context: props_to_wire(decision.context.as_ref()),
    }
}

impl AppPluginApi {
    /// Port of `pluginAccessControlScopeCheck` (plugin_access_control.go:29): the type's format,
    /// then that its plugin-id prefix is this plugin's.
    fn access_scope_check(
        &self,
        where_: &'static str,
        resource_type: &str,
    ) -> Result<(), Box<AppError>> {
        if !is_plugin_access_control_policy_type(resource_type) {
            return Err(AppError::boxed(
                where_,
                "app.access_control.plugin.invalid_resource_type.app_error",
                None,
                resource_type,
                400,
            ));
        }
        if !plugin_owns_access_control_policy_type(&self.id, resource_type) {
            return Err(AppError::boxed(
                where_,
                "app.access_control.plugin.resource_type_forbidden.app_error",
                None,
                format!("plugin_id={}", self.id),
                403,
            ));
        }
        Ok(())
    }

    /// Port of `validatePluginActingUser` (plugin_access_control.go:49): a well-formed id of a
    /// user that exists; either failure is the same 400.
    async fn access_acting_user(
        &self,
        where_: &'static str,
        acting_user_id: &str,
    ) -> Result<(), Box<AppError>> {
        let refuse = || {
            AppError::boxed(
                where_,
                "app.access_control.plugin.invalid_acting_user.app_error",
                None,
                String::new(),
                400,
            )
        };
        if !is_valid_id(acting_user_id) {
            return Err(refuse());
        }
        self.app
            .get_user(acting_user_id)
            .await
            .map_err(|_| refuse())?;
        Ok(())
    }

    /// The scope check and the acting user, in that order.
    async fn access_preamble(
        &self,
        where_: &'static str,
        acting_user_id: &str,
        resource_type: &str,
    ) -> Result<(), Box<AppError>> {
        self.access_scope_check(where_, resource_type)?;
        self.access_acting_user(where_, acting_user_id).await
    }

    /// Port of `resolvePluginPolicyExistence` (plugin_access_control.go:68) with the reason a nil
    /// engine gives, `abac_unavailable`.
    async fn resolve_plugin_policy_existence(
        &self,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<AccessDecision, Box<AppError>> {
        let unavailable = || {
            AppError::boxed(
                "EvaluatePluginAccessRequest",
                "app.access_control.plugin.evaluation_unavailable.app_error",
                None,
                "reason=abac_unavailable",
                503,
            )
        };
        match self
            .app
            .store()
            .access_control_policy()
            .get(resource_id)
            .await
        {
            Err(StoreError::NotFound { .. }) => Ok(AccessDecision::new_no_policy()),
            Err(err) => {
                tracing::warn!(plugin_id = %self.id, resource_id, error = %err, "Plugin access evaluation: existence fallback store read failed");
                Err(unavailable())
            }
            Ok(policy) if policy.type_ != resource_type => Ok(AccessDecision::new_no_policy()),
            Ok(_) => Err(unavailable()),
        }
    }

    /// Port of `PluginAPI.EvaluateAccessControl` → `App.EvaluatePluginAccessRequest`
    /// (plugin_access_control.go:107).
    pub(super) async fn access_evaluate(
        &self,
        args: api::Z_EvaluateAccessControlArgs,
    ) -> Result<api::Z_EvaluateAccessControlReturns, NotImplemented> {
        const WHERE: &str = "EvaluatePluginAccessRequest";
        let (user_id, resource_type, resource_id, action) = (&args.a, &args.b, &args.c, &args.d);
        let answer = async {
            self.access_scope_check(WHERE, resource_type)?;
            if !is_valid_policy_action(action) {
                return Err(AppError::boxed(
                    WHERE,
                    "app.access_control.plugin.invalid_action.app_error",
                    None,
                    format!("action={action}"),
                    400,
                ));
            }
            if !is_valid_id(user_id) || !is_valid_id(resource_id) {
                return Err(invalid_id(WHERE, ""));
            }
            self.resolve_plugin_policy_existence(resource_type, resource_id)
                .await
        }
        .await;
        Ok(match answer {
            Ok(decision) => api::Z_EvaluateAccessControlReturns {
                a: Some(Box::new(decision_to_wire(&decision))),
                b: None,
            },
            Err(err) => api::Z_EvaluateAccessControlReturns {
                a: None,
                b: self.wire(err),
            },
        })
    }

    /// Port of `PluginAPI.SaveAccessControlPolicy` (plugin_access_control.go:163): the engine
    /// is checked before anything else.
    pub(super) async fn access_save_policy(
        &self,
    ) -> Result<api::Z_SaveAccessControlPolicyReturns, NotImplemented> {
        Ok(api::Z_SaveAccessControlPolicyReturns {
            a: None,
            b: self.wire(pap_not_initialized(
                "SavePluginAccessControlPolicy",
                "app.pap.create_access_control_policy.app_error",
            )),
        })
    }

    /// Port of `PluginAPI.GetAccessControlPolicy` (plugin_access_control.go:263).
    pub(super) async fn access_get_policy(
        &self,
    ) -> Result<api::Z_GetAccessControlPolicyReturns, NotImplemented> {
        Ok(api::Z_GetAccessControlPolicyReturns {
            a: None,
            b: self.wire(pap_not_initialized(
                "GetPluginAccessControlPolicy",
                "app.pap.get_policy.app_error",
            )),
        })
    }

    /// Port of `PluginAPI.DeleteAccessControlPolicy` (plugin_access_control.go:314).
    pub(super) async fn access_delete_policy(
        &self,
    ) -> Result<api::Z_DeleteAccessControlPolicyReturns, NotImplemented> {
        Ok(api::Z_DeleteAccessControlPolicyReturns {
            a: self.wire(pap_not_initialized(
                "DeletePluginAccessControlPolicy",
                "app.pap.delete_policy.app_error",
            )),
        })
    }

    /// Port of `PluginAPI.CheckAccessControlExpression` (plugin_access_control.go:365): the
    /// preamble, then `App.CheckExpression`'s 501.
    pub(super) async fn access_check_expression(
        &self,
        args: api::Z_CheckAccessControlExpressionArgs,
    ) -> Result<api::Z_CheckAccessControlExpressionReturns, NotImplemented> {
        let err = match self
            .access_preamble("CheckPluginAccessControlExpression", &args.a, &args.b)
            .await
        {
            Err(err) => err,
            Ok(()) => pap_not_initialized("CheckExpression", "app.pap.check_expression.app_error"),
        };
        Ok(api::Z_CheckAccessControlExpressionReturns {
            a: Vec::new(),
            b: self.wire(err),
        })
    }

    /// Port of `PluginAPI.QueryUsersForAccessControlExpression` (plugin_access_control.go:379):
    /// the preamble, then `App.TestExpression`'s 501 (the limit clamp before it changes nothing).
    pub(super) async fn access_query_users(
        &self,
        args: api::Z_QueryUsersForAccessControlExpressionArgs,
    ) -> Result<api::Z_QueryUsersForAccessControlExpressionReturns, NotImplemented> {
        let err = match self
            .access_preamble(
                "QueryUsersForPluginAccessControlExpression",
                &args.a,
                &args.b,
            )
            .await
        {
            Err(err) => err,
            Ok(()) => pap_not_initialized("TestExpression", "app.pap.check_expression.app_error"),
        };
        Ok(api::Z_QueryUsersForAccessControlExpressionReturns {
            a: None,
            b: self.wire(err),
        })
    }

    /// Port of `PluginAPI.GetAccessControlFieldsAutocomplete` (plugin_access_control.go:409):
    /// the engine first, before the acting user.
    pub(super) async fn access_fields_autocomplete(
        &self,
    ) -> Result<api::Z_GetAccessControlFieldsAutocompleteReturns, NotImplemented> {
        Ok(api::Z_GetAccessControlFieldsAutocompleteReturns {
            a: Vec::new(),
            b: self.wire(pap_not_initialized(
                "GetPluginAccessControlFieldsAutocomplete",
                "app.pap.get_access_control_auto_complete.app_error",
            )),
        })
    }

    /// Port of `PluginAPI.GetAccessControlVisualAST` (plugin_access_control.go:435): the
    /// preamble, then `App.ExpressionToVisualAST`'s 501.
    pub(super) async fn access_visual_ast(
        &self,
        args: api::Z_GetAccessControlVisualASTArgs,
    ) -> Result<api::Z_GetAccessControlVisualASTReturns, NotImplemented> {
        let err = match self
            .access_preamble("GetPluginAccessControlVisualAST", &args.a, &args.b)
            .await
        {
            Err(err) => err,
            Ok(()) => pap_not_initialized(
                "ExpressionToVisualAST",
                "app.pap.expression_to_visual_ast.app_error",
            ),
        };
        Ok(api::Z_GetAccessControlVisualASTReturns {
            a: None,
            b: self.wire(err),
        })
    }
}
