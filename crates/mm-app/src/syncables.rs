//! Port of the syncable half of `app/group.go` — `GetGroupSyncable`, `UpsertGroupSyncable`,
//! `UpdateGroupSyncable`, `DeleteGroupSyncable` (app/group.go:377-575) — and of
//! `app/syncables.go`: the membership sync a link or unlink dispatches after its response.
//!
//! # The sync runs after the response, on both servers
//!
//! `linkGroupSyncable` writes its 201 and then `c.App.Srv().Go(func() { SyncRolesAndMembership })`
//! (api4/group.go:410); the same for `patch`, and `unlink` dispatches
//! `RemoveMembershipsFromUnlinkedSyncable`. Nothing about the sync's outcome is in the response,
//! and its failures are **logged, never returned** — every branch of `syncables.go` ends in
//! `rctx.Logger().Warn`. So the port runs them in a `tokio::spawn`ed task from the handler, and
//! the functions here return nothing a caller could act on, exactly as Go's do. A parity test
//! sees the sync only by polling for its effect.
//!
//! # The upsert has a hidden second write
//!
//! Linking a group to a **channel** first links it to the channel's **team** (`UpsertGroupSyncable`,
//! app/group.go:414): a team that is not group-constrained gets a `GroupTeams` row upserted with
//! the same `AutoAdd`, and one that *is* constrained refuses the channel link unless the group is
//! already among its groups (`group_not_associated_to_synced_team`, **400**). That recursive
//! upsert publishes its own `received_group_associated_to_team` event, so one channel link is two
//! rows and two events.
//!
//! # What "not found" means on each write
//!
//! - `UpdateGroupSyncable`'s store miss is wrapped by `errors.Wrap`, which `errors.As` for
//!   `*AppError` does not see through — so a patch of a row that vanished is **500**
//!   `app.update_error`, not a 404. The handler's own `GetGroupSyncable` in front of it is what
//!   makes that unreachable over HTTP.
//! - `DeleteGroupSyncable` distinguishes the row that is not there (404 `app.group.no_rows`) from
//!   the row already deleted (400 `app.group.group_syncable_already_deleted`).
//! - `CreateGroupSyncable`'s miss is the **team or channel** missing, and it is reported with
//!   `store.sql_channel.get.existing.app_error` for either kind.

use std::collections::HashMap;

use mm_model::group::{CreateDefaultMembershipParams, GroupSource};
use mm_model::group_syncable::{GroupSyncable, GroupSyncableType};
use mm_model::job::{JOB_STATUS_SUCCESS, JOB_TYPE_LDAP_SYNC};
use mm_model::utils::{AppError, AppResult};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_RECEIVED_GROUP_ASSOCIATED_TO_CHANNEL,
    WEBSOCKET_EVENT_RECEIVED_GROUP_ASSOCIATED_TO_TEAM,
    WEBSOCKET_EVENT_RECEIVED_GROUP_NOT_ASSOCIATED_TO_CHANNEL,
    WEBSOCKET_EVENT_RECEIVED_GROUP_NOT_ASSOCIATED_TO_TEAM, WebSocketEvent,
};
use mm_store::{ChannelStore, GroupSyncableStore, JobStore, StoreError, TeamStore};

use crate::App;
use crate::channel_member::{ChannelMemberOpts, MemberWrite};

/// Go's `multierror.Error` from a sync loop: every failure the loop met, or nothing. Only ever
/// logged, so the messages are what matter and they are Go's.
struct SyncErrors(Vec<String>);

impl SyncErrors {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn push(&mut self, message: String) {
        self.0.push(message);
    }

    fn error_or_nil(self) -> Result<(), String> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(self.0.join("; "))
        }
    }
}

fn store_error(where_: &'static str, id: &'static str, err: StoreError) -> Box<AppError> {
    AppError::boxed(where_, id, None, err.to_string(), 500)
}

/// `NewWebSocketEvent(received_group_[not_]associated_to_team, syncable, "", …)` or the channel
/// twin, with `group_id` — the one event both the upsert and the delete publish.
fn syncable_event(gs: &GroupSyncable, associated: bool) -> WebSocketEvent {
    let mut event = if gs.type_.as_str() == GroupSyncableType::TEAM {
        WebSocketEvent::new(
            if associated {
                WEBSOCKET_EVENT_RECEIVED_GROUP_ASSOCIATED_TO_TEAM
            } else {
                WEBSOCKET_EVENT_RECEIVED_GROUP_NOT_ASSOCIATED_TO_TEAM
            },
            &gs.syncable_id,
            "",
            "",
            None,
            "",
        )
    } else {
        WebSocketEvent::new(
            if associated {
                WEBSOCKET_EVENT_RECEIVED_GROUP_ASSOCIATED_TO_CHANNEL
            } else {
                WEBSOCKET_EVENT_RECEIVED_GROUP_NOT_ASSOCIATED_TO_CHANNEL
            },
            "",
            &gs.syncable_id,
            "",
            None,
            "",
        )
    };
    event.add("group_id", serde_json::Value::String(gs.group_id.clone()));
    event
}

impl App {
    /// Port of `App.GetGroupSyncable` (app/group.go:472): 404 `app.group.no_rows` on a miss,
    /// 500 `app.select_error` otherwise. A soft-deleted row is **found** — the store has no
    /// `DeleteAt` filter here — which is what lets a re-link start from the old row's existence.
    #[tracing::instrument(skip(self), fields(found))]
    pub async fn get_group_syncable(
        &self,
        group_id: &str,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> AppResult<GroupSyncable> {
        let gs = self
            .store()
            .group()
            .get_group_syncable(group_id, syncable_id, syncable_type)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetGroupSyncable",
                        "app.group.no_rows",
                        None,
                        err.to_string(),
                        404,
                    )
                } else {
                    store_error("GetGroupSyncable", "app.select_error", err)
                }
            })?;
        tracing::Span::current().record("found", true);
        Ok(gs)
    }

    /// Port of `App.UpsertGroupSyncable` (app/group.go:377). See the module note for the second
    /// write a channel link makes.
    ///
    /// `Box::pin` because the channel arm calls this for the parent team: Go's recursion, one
    /// level deep by construction (a team has no parent).
    pub fn upsert_group_syncable<'a>(
        &'a self,
        group_syncable: GroupSyncable,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<GroupSyncable>> + Send + 'a>>
    {
        Box::pin(self.upsert_group_syncable_inner(group_syncable))
    }

    #[tracing::instrument(skip_all, fields(group_id = %group_syncable.group_id, syncable_id = %group_syncable.syncable_id, syncable_type = %group_syncable.type_, created))]
    async fn upsert_group_syncable_inner(
        &self,
        group_syncable: GroupSyncable,
    ) -> AppResult<GroupSyncable> {
        let store = self.store();
        let existing = match store
            .group()
            .get_group_syncable(
                &group_syncable.group_id,
                &group_syncable.syncable_id,
                &group_syncable.type_,
            )
            .await
        {
            Ok(gs) => Some(gs),
            Err(err) if err.is_not_found() => None,
            Err(err) => return Err(store_error("UpsertGroupSyncable", "app.select_error", err)),
        };

        // reject the syncable creation if the group isn't already associated to the parent team
        if group_syncable.type_.as_str() == GroupSyncableType::CHANNEL {
            let channel = store
                .channel()
                .get(&group_syncable.syncable_id)
                .await
                .map_err(|err| {
                    let params = HashMap::from([(
                        "channel_id".to_owned(),
                        serde_json::Value::String(group_syncable.syncable_id.clone()),
                    )]);
                    if err.is_not_found() {
                        AppError::boxed(
                            "UpsertGroupSyncable",
                            "app.channel.get.existing.app_error",
                            Some(params),
                            err.to_string(),
                            404,
                        )
                    } else {
                        AppError::boxed(
                            "UpsertGroupSyncable",
                            "app.channel.get.find.app_error",
                            Some(params),
                            err.to_string(),
                            500,
                        )
                    }
                })?;

            let team = store.team().get(&channel.team_id).await.map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "UpsertGroupSyncable",
                        "app.team.get.find.app_error",
                        None,
                        err.to_string(),
                        404,
                    )
                } else {
                    AppError::boxed(
                        "UpsertGroupSyncable",
                        "app.team.get.finding.app_error",
                        None,
                        err.to_string(),
                        500,
                    )
                }
            })?;

            if team.is_group_constrained() {
                let permitted = store
                    .group()
                    .group_ids_synced_to_team(&channel.team_id)
                    .await
                    .map_err(|err| store_error("UpsertGroupSyncable", "app.select_error", err))?;
                if !permitted.contains(&group_syncable.group_id) {
                    return Err(AppError::boxed(
                        "UpsertGroupSyncable",
                        "group_not_associated_to_synced_team",
                        None,
                        String::new(),
                        400,
                    ));
                }
            } else {
                self.upsert_group_syncable(GroupSyncable::new_group_team(
                    &group_syncable.group_id,
                    &team.id,
                    group_syncable.auto_add,
                ))
                .await?;
            }
        }

        let gs = if existing.is_none() {
            tracing::Span::current().record("created", true);
            store
                .group()
                .create_group_syncable(group_syncable)
                .await
                .map_err(|err| match err {
                    StoreError::Invalid { app_error, .. } => app_error,
                    err if err.is_not_found() => AppError::boxed(
                        "UpsertGroupSyncable",
                        "store.sql_channel.get.existing.app_error",
                        None,
                        err.to_string(),
                        404,
                    ),
                    err => store_error("UpsertGroupSyncable", "app.insert_error", err),
                })?
        } else {
            tracing::Span::current().record("created", false);
            store
                .group()
                .update_group_syncable(group_syncable)
                .await
                .map_err(|err| match err {
                    StoreError::Invalid { app_error, .. } => app_error,
                    err => store_error("UpsertGroupSyncable", "app.update_error", err),
                })?
        };

        self.publish(syncable_event(&gs, true)).await;
        Ok(gs)
    }

    /// Port of `App.UpdateGroupSyncable` (app/group.go:496).
    ///
    /// A **live** row (`DeleteAt == 0`) is written straight through the store; a deleted one goes
    /// through the upsert "to ensure that there's an associated GroupTeam" — Go's comment has the
    /// two cases backwards ("updating a *deleted* GroupSyncable" on the `DeleteAt == 0` branch),
    /// and the code is what is reproduced.
    #[tracing::instrument(skip_all, fields(group_id = %group_syncable.group_id, syncable_id = %group_syncable.syncable_id, syncable_type = %group_syncable.type_))]
    pub async fn update_group_syncable(
        &self,
        group_syncable: GroupSyncable,
    ) -> AppResult<GroupSyncable> {
        if group_syncable.delete_at == 0 {
            return self
                .store()
                .group()
                .update_group_syncable(group_syncable)
                .await
                .map_err(|err| match err {
                    StoreError::Invalid { app_error, .. } => app_error,
                    err => store_error("UpdateGroupSyncable", "app.update_error", err),
                });
        }
        self.upsert_group_syncable(group_syncable).await
    }

    /// Port of `App.DeleteGroupSyncable` (app/group.go:522): a soft delete, and for a **team**
    /// the soft delete of every live channel syncable of the group with it — each of those
    /// silently, with no event of its own; only the team's `received_group_not_associated_to_team`
    /// is published.
    #[tracing::instrument(skip(self), fields(cascaded))]
    pub async fn delete_group_syncable(
        &self,
        group_id: &str,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> AppResult<GroupSyncable> {
        let delete_error = |err: StoreError| {
            if err.is_not_found() {
                AppError::boxed(
                    "DeleteGroupSyncable",
                    "app.group.no_rows",
                    None,
                    err.to_string(),
                    404,
                )
            } else if err.is_invalid_input() {
                AppError::boxed(
                    "DeleteGroupSyncable",
                    "app.group.group_syncable_already_deleted",
                    None,
                    err.to_string(),
                    400,
                )
            } else {
                store_error("DeleteGroupSyncable", "app.update_error", err)
            }
        };

        let gs = self
            .store()
            .group()
            .delete_group_syncable(group_id, syncable_id, syncable_type)
            .await
            .map_err(delete_error)?;

        // if a GroupTeam is being deleted delete all associated GroupChannels
        if gs.type_.as_str() == GroupSyncableType::TEAM {
            let channel_type = GroupSyncableType::from(GroupSyncableType::CHANNEL);
            let all_group_channels = self
                .store()
                .group()
                .get_all_group_syncables_by_group_id(&gs.group_id, &channel_type)
                .await
                .map_err(|err| store_error("DeleteGroupSyncable", "app.select_error", err))?;
            tracing::Span::current().record("cascaded", all_group_channels.len());
            for group_channel in &all_group_channels {
                self.store()
                    .group()
                    .delete_group_syncable(
                        &group_channel.group_id,
                        &group_channel.syncable_id,
                        &group_channel.type_,
                    )
                    .await
                    .map_err(delete_error)?;
            }
        }

        self.publish(syncable_event(&gs, false)).await;
        Ok(gs)
    }

    /// Port of `App.SyncSyncableRoles` (app/syncables.go:223): make the members of the
    /// syncable's `SchemeAdmin` groups its scheme admins and nobody else, and tell each changed
    /// member's clients through `member_role_updated` / `channel_member_updated`.
    ///
    /// Each changed member's `ClearSessionCacheForUser` is [`App::clear_session_cache_for_user`].
    #[tracing::instrument(skip(self), fields(permitted_admins, updated))]
    pub async fn sync_syncable_roles(
        &self,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> AppResult<()> {
        let permitted_admins = self
            .store()
            .group()
            .permitted_syncable_admins(syncable_id, syncable_type)
            .await
            .map_err(|err| store_error("SyncSyncableRoles", "app.select_error", err))?;
        tracing::Span::current().record("permitted_admins", permitted_admins.len());
        tracing::info!(
            syncable_id,
            ?permitted_admins,
            "Permitted admins for {syncable_type}"
        );

        match syncable_type.as_str() {
            GroupSyncableType::TEAM => {
                let updated = self
                    .store()
                    .team()
                    .update_members_role(syncable_id, &permitted_admins)
                    .await
                    .map_err(|err| store_error("App.SyncSyncableRoles", "app.update_error", err))?;
                tracing::Span::current().record("updated", updated.len());
                for member in &updated {
                    // syncables.go:244
                    self.clear_session_cache_for_user(&member.user_id);
                    self.send_updated_team_member_event(member).await;
                }
            }
            GroupSyncableType::CHANNEL => {
                let updated = self
                    .store()
                    .channel()
                    .update_members_role(syncable_id, &permitted_admins)
                    .await
                    .map_err(|err| store_error("App.SyncSyncableRoles", "app.update_error", err))?;
                tracing::Span::current().record("updated", updated.len());
                for member in &updated {
                    // syncables.go:258
                    self.clear_session_cache_for_user(&member.user_id);
                    self.send_update_channel_member_event(member).await;
                }
            }
            other => {
                return Err(AppError::boxed(
                    "App.SyncSyncableRoles",
                    "groups.unsupported_syncable_type",
                    Some(HashMap::from([(
                        "Value".to_owned(),
                        serde_json::Value::String(other.to_owned()),
                    )])),
                    String::new(),
                    500,
                ));
            }
        }
        Ok(())
    }

    /// Port of `App.SyncRolesAndMembership` (app/syncables.go:273): the roles when asked, then
    /// the memberships of the one syncable. Every failure is a warning, as in Go.
    ///
    /// For an LDAP group `since` is the start of the last successful LDAP sync job (none: 0) and
    /// `reAddRemovedMembers` is the setting; for any other source the sync starts from zero and
    /// re-adds. Go reads the job with `lastJob, _ :=` — an error there is the same as no job.
    #[tracing::instrument(skip(self), fields(since, re_add_removed_members))]
    pub async fn sync_roles_and_membership(
        &self,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
        group_id: &str,
        sync_roles: bool,
    ) {
        let group = match self.get_group(group_id).await {
            Ok(group) => group,
            Err(err) => {
                tracing::warn!(error = %err, "Error getting group");
                return;
            }
        };

        if sync_roles {
            if let Err(err) = self.sync_syncable_roles(syncable_id, syncable_type).await {
                tracing::warn!(error = %err, "Error syncing syncable roles");
            }
        }

        let mut since = 0;
        let mut re_add_removed_members = true;
        if group.source.as_str() == GroupSource::LDAP {
            if let Ok(last_job) = self
                .store()
                .job()
                .get_newest_job_by_status_and_type(JOB_STATUS_SUCCESS, JOB_TYPE_LDAP_SYNC)
                .await
            {
                since = last_job.start_at;
            }
            re_add_removed_members = self.config().ldap_re_add_removed_members;
        }
        tracing::Span::current().record("since", since);
        tracing::Span::current().record("re_add_removed_members", re_add_removed_members);

        let mut params = CreateDefaultMembershipParams {
            since,
            re_add_removed_members,
            ..CreateDefaultMembershipParams::default()
        };
        match syncable_type.as_str() {
            GroupSyncableType::TEAM => {
                params.scoped_team_id = Some(syncable_id.to_owned());
                if let Err(err) = self.create_default_team_memberships(&params).await {
                    tracing::warn!(error = %err, "Error creating default team memberships");
                }
            }
            GroupSyncableType::CHANNEL => {
                params.scoped_channel_id = Some(syncable_id.to_owned());
                if let Err(err) = self.create_default_channel_memberships(&params).await {
                    tracing::warn!(error = %err, "Error creating default channel memberships");
                }
            }
            _ => {}
        }
    }

    /// Port of `App.RemoveMembershipsFromUnlinkedSyncable` (app/syncables.go:315).
    #[tracing::instrument(skip(self))]
    pub async fn remove_memberships_from_unlinked_syncable(
        &self,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) {
        match syncable_type.as_str() {
            GroupSyncableType::TEAM => {
                if let Err(err) = self
                    .delete_group_constrained_team_memberships(Some(syncable_id))
                    .await
                {
                    tracing::warn!(error = %err, "Error deleting group constrained team memberships");
                }
            }
            GroupSyncableType::CHANNEL => {
                if let Err(err) = self
                    .delete_group_constrained_channel_memberships(Some(syncable_id))
                    .await
                {
                    tracing::warn!(error = %err, "Error deleting group constrained channel memberships");
                }
            }
            _ => {}
        }
    }

    /// Port of `App.createDefaultTeamMemberships` (app/syncables.go:91). A refusal for the
    /// team's allowed domains is an info line, not an error; everything else is collected.
    #[tracing::instrument(skip_all, fields(candidates, added))]
    async fn create_default_team_memberships(
        &self,
        params: &CreateDefaultMembershipParams,
    ) -> Result<(), String> {
        let team_members = self
            .store()
            .group()
            .team_members_to_add(
                params.since,
                params.scoped_team_id.as_deref(),
                params.re_add_removed_members,
            )
            .await
            .map_err(|err| store_error("TeamMembersToAdd", "app.select_error", err).to_string())?;
        tracing::Span::current().record("candidates", team_members.len());

        let mut errors = SyncErrors::new();
        let mut added = 0;
        for user_team in &team_members {
            if params
                .scoped_user_id
                .as_deref()
                .is_some_and(|scoped| scoped != user_team.user_id)
            {
                continue;
            }
            match self
                .add_team_member(&user_team.team_id, &user_team.user_id)
                .await
            {
                Ok(_) => {
                    added += 1;
                    tracing::info!(user_id = %user_team.user_id, team_id = %user_team.team_id, "Added team member for default team membership");
                }
                Err(err) if err.id == "api.team.join_user_to_team.allowed_domains.app_error" => {
                    tracing::info!(user_id = %user_team.user_id, team_id = %user_team.team_id, "User not added to team - the domain associated with the user is not in the list of allowed team domains");
                }
                Err(err) => errors.push(format!(
                    "failed to add team member for default team membership: {err}"
                )),
            }
        }
        tracing::Span::current().record("added", added);
        errors.error_or_nil()
    }

    /// Port of `App.createDefaultChannelMemberships` (app/syncables.go:22): the team first, if
    /// the user is not yet in it, then the channel with the team-integrity check skipped.
    ///
    /// `AddChannelMember` can answer with a hand-over on this side (`MemberWrite::Forward`) for a
    /// case it cannot reproduce; in a background task there is nobody to hand over to, so it is
    /// logged as a warning and the user is not added.
    #[tracing::instrument(skip_all, fields(candidates, added))]
    async fn create_default_channel_memberships(
        &self,
        params: &CreateDefaultMembershipParams,
    ) -> Result<(), String> {
        let channel_members = self
            .store()
            .group()
            .channel_members_to_add(
                params.since,
                params.scoped_channel_id.as_deref(),
                params.re_add_removed_members,
            )
            .await
            .map_err(|err| {
                store_error("ChannelMembersToAdd", "app.select_error", err).to_string()
            })?;
        tracing::Span::current().record("candidates", channel_members.len());

        let mut errors = SyncErrors::new();
        let mut added = 0;
        for user_channel in &channel_members {
            if params
                .scoped_user_id
                .as_deref()
                .is_some_and(|scoped| scoped != user_channel.user_id)
            {
                continue;
            }
            let channel = match self.get_channel(&user_channel.channel_id).await {
                Ok(channel) => channel,
                Err(err) => {
                    errors.push(format!(
                        "failed to get channel for default channel membership: {err}"
                    ));
                    continue;
                }
            };
            let team_member = match self
                .get_team_member(&channel.team_id, &user_channel.user_id)
                .await
            {
                Ok(member) => Some(member),
                Err(err) if err.id == "app.team.get_member.missing.app_error" => None,
                Err(err) => {
                    errors.push(format!(
                        "failed to get member for default channel membership: {err}"
                    ));
                    continue;
                }
            };
            // First add user to team
            if team_member.is_none() {
                match self
                    .add_team_member(&channel.team_id, &user_channel.user_id)
                    .await
                {
                    Ok(_) => {
                        tracing::info!(user_id = %user_channel.user_id, channel_id = %user_channel.channel_id, "Added channel member for default channel membership");
                    }
                    Err(err) => {
                        if err.id == "api.team.join_user_to_team.allowed_domains.app_error" {
                            tracing::info!(user_id = %user_channel.user_id, team_id = %channel.team_id, "User not added to channel - the domain associated with the user is not in the list of allowed team domains");
                        } else {
                            errors.push(format!(
                                "failed to add team member for default channel membership: {err}"
                            ));
                        }
                        continue;
                    }
                }
            }
            let opts = ChannelMemberOpts {
                skip_team_member_integrity_check: true,
                ..ChannelMemberOpts::default()
            };
            match self
                .add_channel_member(&user_channel.user_id, &channel, &opts)
                .await
            {
                Ok(MemberWrite::Done(_)) => {
                    added += 1;
                    tracing::info!(user_id = %user_channel.user_id, channel_id = %user_channel.channel_id, "Added channel member for default channel membership");
                }
                Ok(MemberWrite::Forward(reason)) => {
                    tracing::warn!(user_id = %user_channel.user_id, channel_id = %user_channel.channel_id, reason, "default channel membership skipped: the add is not reproducible here");
                }
                Err(err) => {
                    if err.id == "api.channel.add_user.to.channel.failed.deleted.app_error" {
                        tracing::info!(user_id = %user_channel.user_id, channel_id = %user_channel.channel_id, "Not adding user to channel because they have already left the team");
                    } else {
                        errors.push(format!(
                            "failed to add channel member for default channel membership: {err}"
                        ));
                    }
                }
            }
        }
        tracing::Span::current().record("added", added);
        errors.error_or_nil()
    }

    /// Port of `App.DeleteGroupConstrainedTeamMemberships` (app/syncables.go:161), with no
    /// requester — Go passes `""`.
    #[tracing::instrument(skip(self), fields(candidates))]
    async fn delete_group_constrained_team_memberships(
        &self,
        team_id: Option<&str>,
    ) -> Result<(), String> {
        let team_members = self
            .store()
            .group()
            .team_members_to_remove(team_id)
            .await
            .map_err(|err| {
                store_error("TeamMembersToRemove", "app.select_error", err).to_string()
            })?;
        tracing::Span::current().record("candidates", team_members.len());
        let mut errors = SyncErrors::new();
        for user_team in &team_members {
            match self
                .remove_user_from_team(&user_team.team_id, &user_team.user_id, "")
                .await
            {
                Ok(()) => {
                    tracing::info!(user_id = %user_team.user_id, team_id = %user_team.team_id, "Removed team member for group constrained team membership");
                }
                Err(err) => errors.push(format!(
                    "failed to remove team member for default team membership: {err}"
                )),
            }
        }
        errors.error_or_nil()
    }

    /// Port of `App.DeleteGroupConstrainedChannelMemberships` (app/syncables.go:189).
    #[tracing::instrument(skip(self), fields(candidates))]
    async fn delete_group_constrained_channel_memberships(
        &self,
        channel_id: Option<&str>,
    ) -> Result<(), String> {
        let channel_members = self
            .store()
            .group()
            .channel_members_to_remove(channel_id)
            .await
            .map_err(|err| {
                store_error("ChannelMembersToRemove", "app.select_error", err).to_string()
            })?;
        tracing::Span::current().record("candidates", channel_members.len());
        let mut errors = SyncErrors::new();
        for user_channel in &channel_members {
            let channel = match self.get_channel(&user_channel.channel_id).await {
                Ok(channel) => channel,
                Err(err) => {
                    errors.push(format!(
                        "failed to get channel for group constrained channel membership: {err}"
                    ));
                    continue;
                }
            };
            match self
                .remove_user_from_channel(&user_channel.user_id, "", &channel)
                .await
            {
                Ok(MemberWrite::Done(())) => {
                    tracing::info!(user_id = %user_channel.user_id, channel_id = %user_channel.channel_id, "Removed channel member for group constrained channel membership");
                }
                Ok(MemberWrite::Forward(reason)) => {
                    tracing::warn!(user_id = %user_channel.user_id, channel_id = %user_channel.channel_id, reason, "group constrained channel membership not removed: the removal is not reproducible here");
                }
                Err(err) => errors.push(format!(
                    "failed to remove channel member for default team membership: {err}"
                )),
            }
        }
        errors.error_or_nil()
    }
}
