//! Port of `app/notify_admin.go`: the rows behind `POST /api/v4/users/notify-admin`
//! (`SaveAdminNotification`), and the send that turns them into posts to the admins
//! (`DoCheckForAdminNotifications`, `SendNotifyAdminPosts`, `CanNotifyAdmin`,
//! `FinishSendAdminNotifyPost`) — the body of the three notify-admin jobs and of
//! `POST /api/v4/users/trigger-notify-admin-posts`.
//!
//! # What the send does
//!
//! After a cool-off (`CanNotifyAdmin`: fourteen days since the last send, or
//! `MM_NOTIFY_ADMIN_COOL_OFF_DAYS`), the unsent rows of the flavour asked for (upgrade or trial)
//! whose plan is not the current SKU are grouped by user, by feature and by plugin. Every system
//! administrator — deactivated ones too, since `GetUsersFromProfiles` is asked with `Inactive`
//! false and no `Active` — gets a `custom_up_notification` post from the system bot in their
//! direct channel when there is at least one non-plugin feature; then the send is stamped in
//! `Systems`, the plugin rows are marked sent and every other unsent row of the flavour created
//! before the send is deleted. No rows at all is a quiet success that stamps nothing.

use std::collections::BTreeMap;

use mm_model::notify_admin::{
    MattermostFeature, NotifyAdminData, NotifyAdminToUpgradeRequest, PLUGIN_FEATURE,
};
use mm_model::post::Post;
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_store::NotifyAdminStore;
use mm_store::system_store::SystemStore;
use mm_store::user_store::UserStore;

use crate::App;
use crate::channel_create::ChannelCreate;
use crate::post_create::CreatePostFlags;

/// `lastTrialNotificationTimeStamp` (notify_admin.go:23).
pub const LAST_TRIAL_NOTIFICATION_TIMESTAMP: &str = "LAST_TRIAL_NOTIFICATION_TIMESTAMP";
/// `lastUpgradeNotificationTimeStamp` (notify_admin.go:24).
pub const LAST_UPGRADE_NOTIFICATION_TIMESTAMP: &str = "LAST_UPGRADE_NOTIFICATION_TIMESTAMP";
/// `defaultNotifyAdminCoolOffDays` (notify_admin.go:25).
const DEFAULT_NOTIFY_ADMIN_COOL_OFF_DAYS: f64 = 14.0;
/// `fmt.Sprintf("%sup_notification", model.PostCustomTypePrefix)`.
pub const POST_TYPE_UP_NOTIFICATION: &str = "custom_up_notification";

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

    /// Port of `App.DoCheckForAdminNotifications` (notify_admin.go:49): the SKU is the licence's
    /// short name, `starter` without one, and the workspace name is always empty.
    pub async fn do_check_for_admin_notifications(&self, trial: bool) -> AppResult<()> {
        let current_sku = match self.license().await? {
            Some(license) => license.sku_short_name.clone(),
            None => "starter".to_owned(),
        };
        self.send_notify_admin_posts(
            &crate::plugin_hooks::HookContext::default(),
            None,
            "",
            &current_sku,
            trial,
        )
        .await
    }

    /// Port of `App.SendNotifyAdminPosts` (notify_admin.go:86). `session` is the request's
    /// (`trigger-notify-admin-posts`) or none (the jobs); it reaches `CreatePost` and
    /// `GetOrCreateDirectChannel`.
    pub async fn send_notify_admin_posts(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        session: Option<&mm_model::session::Session>,
        workspace_name: &str,
        current_sku: &str,
        trial: bool,
    ) -> AppResult<()> {
        if !self.can_notify_admin(trial).await {
            return Err(AppError::boxed(
                "SendNotifyAdminPosts",
                "app.notify_admin.send_notification_post.app_error",
                None,
                "Cannot notify yet".to_owned(),
                403,
            ));
        }

        let pattern = format!(
            "%{}%",
            mm_store::user_store::sanitize_search_term(mm_model::role::SYSTEM_ADMIN_ROLE_ID, '\\')
        );
        let mut sysadmins = self
            .store()
            .user()
            .get_all_profiles_in_role(&pattern, 0, 100)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "the administrator lookup failed");
                AppError::boxed(
                    "GetUsers",
                    "app.user.get_profiles.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        let none = std::collections::HashMap::new();
        for user in &mut sysadmins {
            user.sanitize(&none);
        }

        let system_bot = self.get_system_bot().await?;
        let now = get_millis();

        let data = self
            .store()
            .notify_admin()
            .get(trial)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "notify-admin rows could not be read");
                AppError::boxed(
                    "SendNotifyAdminPosts",
                    "app.notify_admin.send_notification_post.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        let data: Vec<NotifyAdminData> = data
            .into_iter()
            .filter(|d| d.required_plan != current_sku)
            .collect();
        if data.is_empty() {
            tracing::warn!("No notification data available");
            return Ok(());
        }

        let by_user = group_by_user(&data);
        let by_feature = group_by_paid_feature(&data);
        let by_plugin = group_by_plugin(&data);

        for admin in &sysadmins {
            if !by_user.is_empty() && !by_feature.is_empty() {
                self.upgrade_plan_admin_notify_post(
                    ctx,
                    session,
                    workspace_name,
                    by_user.len(),
                    &by_feature,
                    &system_bot.user_id,
                    admin,
                    trial,
                )
                .await;
            }
        }

        self.finish_send_admin_notify_post(trial, now, &by_plugin)
            .await;
        Ok(())
    }

    /// Port of `App.upgradePlanAdminNotifyPost` (notify_admin.go:133): the message in the
    /// admin's own locale, then the post. Every failure is logged, never returned.
    #[allow(clippy::too_many_arguments)]
    async fn upgrade_plan_admin_notify_post(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        session: Option<&mm_model::session::Session>,
        workspace_name: &str,
        users_num: usize,
        by_feature: &BTreeMap<String, Vec<&NotifyAdminData>>,
        bot_user_id: &str,
        admin: &User,
        trial: bool,
    ) {
        let id = match (trial, users_num == 1) {
            (false, false) => "app.cloud.upgrade_plan_bot_message",
            (false, true) => "app.cloud.upgrade_plan_bot_message_single",
            (true, false) => "app.cloud.trial_plan_bot_message",
            (true, true) => "app.cloud.trial_plan_bot_message_single",
        };
        let params: crate::i18n::Params = [
            ("UsersNum".to_owned(), serde_json::Value::from(users_num)),
            (
                "WorkspaceName".to_owned(),
                serde_json::Value::String(workspace_name.to_owned()),
            ),
        ]
        .into_iter()
        .collect();
        let message = match crate::i18n::loaded() {
            Some(bundle) => {
                bundle.translate_with(bundle.user_locale(&admin.locale), id, Some(&params))
            }
            None => id.to_owned(),
        };

        let channel = match self
            .get_or_create_direct_channel(ctx, session, bot_user_id, &admin.id)
            .await
        {
            Ok(ChannelCreate::Created(channel)) => channel,
            Ok(ChannelCreate::Forward(reason)) => {
                tracing::warn!(
                    reason,
                    "Error getting direct channel: not reproducible here"
                );
                return;
            }
            Err(err) => {
                tracing::warn!(error = %err, "Error getting direct channel");
                return;
            }
        };

        let requested: serde_json::Map<String, serde_json::Value> = by_feature
            .iter()
            .map(|(feature, rows)| {
                (
                    feature.clone(),
                    serde_json::to_value(rows).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect();
        let mut post = Post {
            message,
            user_id: bot_user_id.to_owned(),
            channel_id: channel.id.clone(),
            post_type: POST_TYPE_UP_NOTIFICATION.to_owned(),
            ..Post::default()
        };
        post.add_prop("requested_features", serde_json::Value::Object(requested));
        post.add_prop("trial", serde_json::Value::Bool(trial));

        let empty = mm_model::session::Session::default();
        if let Err(err) = self
            .create_post(
                post,
                &channel,
                session.unwrap_or(&empty),
                CreatePostFlags {
                    set_online: true,
                    ..CreatePostFlags::default()
                },
                ctx,
            )
            .await
        {
            tracing::warn!(error = %err, "Error creating post");
        }
    }

    /// Port of `App.CanNotifyAdmin` (notify_admin.go:185): no stamp yet is yes; an unreadable
    /// stamp is no; otherwise the cool-off in days (`MM_NOTIFY_ADMIN_COOL_OFF_DAYS`, fourteen when
    /// unset or unparsable) must have passed.
    pub async fn can_notify_admin(&self, trial: bool) -> bool {
        let name = if trial {
            LAST_TRIAL_NOTIFICATION_TIMESTAMP
        } else {
            LAST_UPGRADE_NOTIFICATION_TIMESTAMP
        };
        let value = match self.store().system().get_by_name(name).await {
            Ok(Some(value)) => value,
            // "if no timestamps have been recorded before, system is free to notify"
            Ok(None) => return true,
            Err(err) => {
                tracing::error!(error = %err, "Cannot notify");
                return false;
            }
        };
        let Ok(last) = value.parse::<f64>() else {
            tracing::error!(value, "Cannot notify");
            return false;
        };
        let days = std::env::var("MM_NOTIFY_ADMIN_COOL_OFF_DAYS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(DEFAULT_NOTIFY_ADMIN_COOL_OFF_DAYS);
        let days_to_millis = days * 24.0 * 60.0 * 60.0 * 1000.0;
        // `int64(float)` truncates toward zero, as `as` does for these magnitudes.
        get_millis() - last as i64 >= days_to_millis as i64
    }

    /// Port of `App.FinishSendAdminNotifyPost` (notify_admin.go:219): stamp the send with a
    /// **fresh** clock read (not `now`), mark every plugin row sent at `now`, and delete the
    /// flavour's other unsent rows created before `now`. Each failure is logged only.
    async fn finish_send_admin_notify_post(
        &self,
        trial: bool,
        now: i64,
        by_plugin: &BTreeMap<String, Vec<&NotifyAdminData>>,
    ) {
        let name = if trial {
            LAST_TRIAL_NOTIFICATION_TIMESTAMP
        } else {
            LAST_UPGRADE_NOTIFICATION_TIMESTAMP
        };
        if let Err(err) = self
            .store()
            .system()
            .save_or_update(name, &get_millis().to_string())
            .await
        {
            tracing::error!(error = %err, "Unable to finish send admin notify post job");
        }

        for rows in by_plugin.values() {
            for row in rows {
                if let Err(err) = self
                    .store()
                    .notify_admin()
                    .update(&row.user_id, &row.required_plan, &row.required_feature, now)
                    .await
                {
                    tracing::error!(error = %err, "Unable to update SentAt for work template feature");
                }
            }
        }

        if let Err(err) = self.store().notify_admin().delete_before(trial, now).await {
            tracing::error!(error = %err, "Unable to finish send admin notify post job");
        }
    }
}

/// `groupNotifyAdminByUser`.
fn group_by_user(data: &[NotifyAdminData]) -> BTreeMap<String, Vec<&NotifyAdminData>> {
    let mut map: BTreeMap<String, Vec<&NotifyAdminData>> = BTreeMap::new();
    for d in data {
        map.entry(d.user_id.clone()).or_default().push(d);
    }
    map
}

/// `groupNotifyAdminByPaidFeature`: every feature not named with the plugin prefix.
fn group_by_paid_feature(data: &[NotifyAdminData]) -> BTreeMap<String, Vec<&NotifyAdminData>> {
    let mut map: BTreeMap<String, Vec<&NotifyAdminData>> = BTreeMap::new();
    for d in data {
        if d.required_feature.as_str().starts_with(PLUGIN_FEATURE) {
            continue;
        }
        map.entry(d.required_feature.as_str().to_owned())
            .or_default()
            .push(d);
    }
    map
}

/// `groupNotifyAdminByPlugin`: a plugin feature's row once per plugin id in its comma-separated
/// plan.
fn group_by_plugin(data: &[NotifyAdminData]) -> BTreeMap<String, Vec<&NotifyAdminData>> {
    let mut map: BTreeMap<String, Vec<&NotifyAdminData>> = BTreeMap::new();
    for d in data {
        if d.required_feature.as_str().starts_with(PLUGIN_FEATURE) {
            for plugin in d.required_plan.split(',') {
                map.entry(plugin.to_owned()).or_default().push(d);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(user: &str, feature: &str, plan: &str) -> NotifyAdminData {
        NotifyAdminData {
            user_id: user.to_owned(),
            required_feature: MattermostFeature(feature.to_owned()),
            required_plan: plan.to_owned(),
            ..NotifyAdminData::default()
        }
    }

    /// The three groupings `SendNotifyAdminPosts` makes: by user (all rows), by paid feature
    /// (plugin features left out), and by plugin (a plugin row under every id in its plan).
    #[test]
    fn rows_are_grouped_as_go_groups_them() {
        let data = vec![
            row("u1", "mattermost.feature.guest_accounts", "professional"),
            row("u2", "mattermost.feature.guest_accounts", "professional"),
            row("u2", "mattermost.feature.plugin.x", "com.a,com.b"),
        ];
        let by_user = group_by_user(&data);
        assert_eq!(by_user.len(), 2);
        assert_eq!(by_user["u2"].len(), 2);

        let by_feature = group_by_paid_feature(&data);
        assert_eq!(
            by_feature.keys().collect::<Vec<_>>(),
            ["mattermost.feature.guest_accounts"]
        );
        assert_eq!(by_feature["mattermost.feature.guest_accounts"].len(), 2);

        let by_plugin = group_by_plugin(&data);
        assert_eq!(by_plugin.keys().collect::<Vec<_>>(), ["com.a", "com.b"]);
        assert_eq!(by_plugin["com.a"][0].user_id, "u2");
    }
}
