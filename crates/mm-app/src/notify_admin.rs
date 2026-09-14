//! Port of `app/notify_admin.go` — the slice behind `POST /api/v4/users/notify-admin`:
//! `SaveAdminNotification`, `UserAlreadyNotifiedOnRequiredFeature` and `SaveAdminNotifyData`.
//!
//! The rest of that file — `DoCheckForAdminNotifications`, `SendNotifyAdminPosts`, the
//! `CanNotifyAdmin` cooldown read from `Systems` — is the job that turns these rows into posts
//! and mails to the admins. It runs on a schedule, not on a request, and is not here.

use mm_model::notify_admin::{MattermostFeature, NotifyAdminData, NotifyAdminToUpgradeRequest};
use mm_model::utils::{AppError, AppResult};
use mm_store::NotifyAdminStore;

use crate::App;

impl App {
    /// Port of `app.App.SaveAdminNotification` (notify_admin.go:39).
    ///
    /// One row per user and feature: a second request for a feature the user already asked
    /// about — under **any** plan, and whether or not the admins were mailed — is the 403
    /// `api.cloud.notify_admin_to_upgrade_error.already_notified`, and no lookup failure
    /// changes that answer (`UserAlreadyNotifiedOnRequiredFeature` reads an error as "not
    /// notified"). Otherwise the row is written as sent, `trial` included.
    #[tracing::instrument(skip(self, request), fields(user_id = %user_id, feature = %request.required_feature))]
    pub async fn save_admin_notification(
        &self,
        user_id: &str,
        request: &NotifyAdminToUpgradeRequest,
    ) -> AppResult<()> {
        if self
            .user_already_notified_on_required_feature(user_id, &request.required_feature)
            .await
        {
            return Err(AppError::boxed(
                "app.SaveAdminNotification",
                "api.cloud.notify_admin_to_upgrade_error.already_notified",
                None,
                String::new(),
                403,
            ));
        }

        self.save_admin_notify_data(&NotifyAdminData {
            user_id: user_id.to_owned(),
            required_plan: request.required_plan.clone(),
            required_feature: request.required_feature.clone(),
            trial: request.trial_notification,
            ..NotifyAdminData::default()
        })
        .await?;
        Ok(())
    }

    /// Port of `app.App.UserAlreadyNotifiedOnRequiredFeature` (notify_admin.go:174): any row
    /// for the user and feature means yes; a store failure means **no**.
    async fn user_already_notified_on_required_feature(
        &self,
        user_id: &str,
        feature: &MattermostFeature,
    ) -> bool {
        match self
            .store()
            .notify_admin()
            .get_data_by_user_id_and_feature(user_id, feature)
            .await
        {
            Ok(data) => !data.is_empty(),
            Err(err) => {
                tracing::warn!(error = %err, "notify-admin lookup failed; treating as not notified");
                false
            }
        }
    }

    /// Port of `app.App.SaveAdminNotifyData` (notify_admin.go:62).
    ///
    /// Every failure is `app.notify_admin.save.app_error`: a 404 for the store's not-found —
    /// which `Save` never produces — and a **500 for everything else, the validator's 400
    /// included**. So an invalid plan or feature reaches the wire as a 500 with the generic id,
    /// and the validator's "Invalid plan, … provided" message goes only to the log.
    async fn save_admin_notify_data(&self, data: &NotifyAdminData) -> AppResult<NotifyAdminData> {
        self.store().notify_admin().save(data).await.map_err(|err| {
            let status = if err.is_not_found() { 404 } else { 500 };
            tracing::warn!(error = %err, status, "notify-admin save failed");
            AppError::boxed(
                "SaveAdminNotifyData",
                "app.notify_admin.save.app_error",
                None,
                String::new(),
                status,
            )
        })
    }
}
