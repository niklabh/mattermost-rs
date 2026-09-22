//! Port of `App.CompleteOnboarding` (app/onboarding.go:30), the app half of
//! `POST /api/v4/system/onboarding/complete`.
//!
//! Outside Cloud the organisation name is required (the 400
//! `api.error_no_organization_name_provided_for_self_hosted_onboarding`) and, when given, saved
//! as the `OrganizationName` system row — its failure only logged. The plugins the request asks
//! for are then installed from the marketplace, each on its own goroutine, and the
//! `FirstAdminSetupComplete` row is written `true`. This port writes the two rows and, when this
//! process hosts plugins, runs the installs ([`App::install_onboarding_plugins`]); under the Go
//! host a request that names plugins is the handler's to forward, since the plugins live there.

use mm_model::system::{SYSTEM_FIRST_ADMIN_SETUP_COMPLETE, SYSTEM_ORGANIZATION_NAME};
use mm_model::utils::{AppError, AppResult};
use mm_store::system_store::SystemStore;

use crate::App;
use crate::plugin_hooks::HookContext;

impl App {
    /// The organisation-name half of `CompleteOnboarding`: the Cloud check, the required-name
    /// refusal, and the `OrganizationName` upsert whose failure is logged.
    #[tracing::instrument(skip_all, fields(cloud))]
    pub async fn save_onboarding_organization(&self, organization: &str) -> AppResult<()> {
        let is_cloud = self
            .license()
            .await?
            .is_some_and(|license| license.is_cloud());
        tracing::Span::current().record("cloud", is_cloud);

        if !is_cloud && organization.is_empty() {
            tracing::error!("No organization name provided for self hosted onboarding");
            return Err(AppError::boxed(
                "CompleteOnboarding",
                "api.error_no_organization_name_provided_for_self_hosted_onboarding",
                None,
                String::new(),
                400,
            ));
        }

        if !organization.is_empty()
            && let Err(err) = self
                .store()
                .system()
                .save_or_update(SYSTEM_ORGANIZATION_NAME, organization)
                .await
        {
            tracing::error!(error = %err, "failed to save organization name");
        }
        Ok(())
    }

    /// Port of `App.markAdminOnboardingComplete` (app/onboarding.go:17): the
    /// `FirstAdminSetupComplete` row written `true`; a store failure is the 500
    /// `api.error_set_first_admin_complete_setup`.
    #[tracing::instrument(skip_all)]
    pub async fn mark_admin_onboarding_complete(&self) -> AppResult<()> {
        self.store()
            .system()
            .save_or_update(SYSTEM_FIRST_ADMIN_SETUP_COMPLETE, "true")
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "first-admin setup flag write failed");
                AppError::boxed(
                    "setFirstAdminCompleteSetup",
                    "api.error_set_first_admin_complete_setup",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// The plugin half of `CompleteOnboarding` (app/onboarding.go:47-85), under the Rust host.
    ///
    /// Does nothing when there is no plugins environment — Go's `GetPluginsEnvironment() == nil`,
    /// which skips every install and goes straight to `markAdminOnboardingComplete` — and
    /// otherwise spawns one task per id, as Go starts one goroutine, and returns without waiting
    /// for any of them. Each task installs the id from the Marketplace, enables it, and
    /// calls **that plugin's** `OnInstall` with `UserId` the session's user; every failure is
    /// only logged, and ends that task. The `plugin.Context` is built before the spawn, as Go
    /// builds it on the request's goroutine.
    ///
    /// `HooksForPlugin` takes the id as the request spelled it, where `EnablePlugin` lowercased
    /// it, so a mixed-case id installs and enables and then finds no hooks.
    #[tracing::instrument(skip_all, fields(plugins = plugin_ids.len()))]
    pub fn install_onboarding_plugins(
        &self,
        ctx: &HookContext,
        user_id: &str,
        plugin_ids: &[String],
    ) {
        if self.plugins_environment().is_none() {
            return;
        }
        for id in plugin_ids {
            // Each task owns what it reads: the spawn outlives the request.
            let (app, ctx, user_id, id) =
                (self.clone(), ctx.clone(), user_id.to_owned(), id.clone());
            tokio::spawn(async move {
                app.install_onboarding_plugin(&ctx, user_id, &id).await;
            });
        }
    }

    /// One goroutine of [`App::install_onboarding_plugins`] (onboarding.go:56-84).
    async fn install_onboarding_plugin(&self, ctx: &HookContext, user_id: String, id: &str) {
        let request = mm_model::marketplace_plugin::InstallMarketplacePluginRequest {
            id: id.to_owned(),
            version: String::new(),
        };
        if let Err(err) = self.install_marketplace_plugin(&request).await {
            tracing::error!(id, error = %err, "Failed to install plugin for onboarding");
            return;
        }
        if let Err(err) = self.enable_plugin(id).await {
            tracing::error!(id, error = %err, "Failed to enable plugin for onboarding");
            return;
        }
        self.on_install(ctx, user_id, id).await;
    }
}
