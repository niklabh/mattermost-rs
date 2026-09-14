//! Port of `App.CompleteOnboarding` (app/onboarding.go:30), the app half of
//! `POST /api/v4/system/onboarding/complete`.
//!
//! Outside Cloud the organisation name is required (the 400
//! `api.error_no_organization_name_provided_for_self_hosted_onboarding`) and, when given, saved
//! as the `OrganizationName` system row — its failure only logged. The plugins the request asks
//! for are then installed from the marketplace, each on its own goroutine, and the
//! `FirstAdminSetupComplete` row is written `true`. This port writes the two rows; a request that
//! names plugins is the handler's to forward, since there is no plugin host here.

use mm_model::system::{SYSTEM_FIRST_ADMIN_SETUP_COMPLETE, SYSTEM_ORGANIZATION_NAME};
use mm_model::utils::{AppError, AppResult};
use mm_store::system_store::SystemStore;

use crate::App;

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
}
