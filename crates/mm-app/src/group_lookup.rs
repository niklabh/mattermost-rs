//! Port of the `App` group functions of `channels/app/group.go` and `app/syncables.go` that only
//! the plugin API reaches: `CreateGroup`, `GetGroupByRemoteID`, `GetGroupsBySource`,
//! `GetGroupsByUserId`, `GetGroupMemberUsersPage`, `UpsertGroupMember`, `DeleteGroupMember`,
//! `GetGroupSyncables`, `GetGroups`, `CreateDefaultMemberships` and
//! `DeleteGroupConstrainedMemberships` (app/plugin_api.go:783-893, :1669-1700).
//!
//! The api4 reads that would also reach several of these are still forwarded when licensed (see
//! `mm_api::groups`); the functions are Go's whole, not the plugin's slice of them.
//!
//! # `CreateGroup` publishes nothing
//!
//! Unlike its three neighbours (`UpdateGroup`, `DeleteGroup`, `RestoreGroup`) and unlike
//! `CreateGroupWithUserIds`, Go's `CreateGroup` neither re-reads the member count nor publishes
//! `received_group`: the group answered is the caller's own, with an id and two clocks set.
//!
//! # The single-member writes are the batch store calls with one id, and their own switches
//!
//! `UpsertMember` is `UpsertMembers(groupID, []string{userID})[0]` and `DeleteMember` builds the
//! same delete as `DeleteMembers`, so the rows and the event are the batch's. The error ids are
//! not: a membership that is not there is **404 `app.group.no_rows`** for `DeleteGroupMember`,
//! where the batch answers 400 `app.group.user_not_found` for the same store error.

use mm_model::group::{CreateDefaultMembershipParams, Group, GroupSearchOpts, GroupSource};
use mm_model::group_member::GroupMember;
use mm_model::group_syncable::{GroupSyncable, GroupSyncableType};
use mm_model::user::{User, ViewUsersRestrictions};
use mm_model::utils::{AppError, AppResult};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_GROUP_MEMBER_ADD, WEBSOCKET_EVENT_GROUP_MEMBER_DELETE,
};
use mm_store::{GroupLookupStore, GroupStore, GroupSyncableStore, StoreError};

use crate::App;
use crate::plugin_hooks::HookContext;

/// `app.select_error` at 500, the generic read failure every function here but one answers.
fn select_error(where_: &str, err: &StoreError) -> Box<AppError> {
    tracing::error!(caller = where_, error = %err, "group read failed");
    AppError::boxed(where_, "app.select_error", None, String::new(), 500)
}

/// `app.group.no_rows` at 404 for a miss, [`select_error`] for anything else — the switch
/// `GetGroup`, `GetGroupByName` and `GetGroupByRemoteID` share.
fn lookup_error(where_: &str, err: &StoreError) -> Box<AppError> {
    if err.is_not_found() {
        AppError::boxed(where_, "app.group.no_rows", None, String::new(), 404)
    } else {
        select_error(where_, err)
    }
}

impl App {
    /// Port of `App.CreateGroup` (app/group.go:109): the name must not be a username (the
    /// refusal's `Where` becomes `CreateGroup`), then the store's plain insert. The store's
    /// `AppError` passes through, an id already set is 400 `app.group.id.app_error`, and every
    /// other failure — a duplicate name among them — is 500 `app.insert_error`.
    #[tracing::instrument(skip_all, fields(name = group.get_name()))]
    pub async fn create_group(&self, group: Group) -> AppResult<Group> {
        self.is_unique_to_usernames(group.get_name())
            .await
            .map_err(|mut err| {
                err.where_ = "CreateGroup".to_owned();
                err
            })?;
        self.store()
            .group()
            .create(group)
            .await
            .map_err(|err| match err {
                StoreError::Invalid { app_error, .. } => app_error,
                StoreError::InvalidInput { .. } => AppError::boxed(
                    "CreateGroup",
                    "app.group.id.app_error",
                    None,
                    String::new(),
                    400,
                ),
                other => {
                    tracing::error!(error = %other, "group create failed");
                    AppError::boxed("CreateGroup", "app.insert_error", None, String::new(), 500)
                }
            })
    }

    /// Port of `App.GetGroupByRemoteID` (app/group.go:68).
    #[tracing::instrument(skip_all, fields(remote_id = %remote_id))]
    pub async fn get_group_by_remote_id(
        &self,
        remote_id: &str,
        source: &GroupSource,
    ) -> AppResult<Group> {
        self.store()
            .group()
            .get_by_remote_id(remote_id, source)
            .await
            .map_err(|err| lookup_error("GetGroupByRemoteID", &err))
    }

    /// Port of `App.GetGroupsBySource` (app/group.go:83).
    #[tracing::instrument(skip_all, fields(source = %source.as_str()))]
    pub async fn get_groups_by_source(&self, source: &GroupSource) -> AppResult<Vec<Group>> {
        self.store()
            .group()
            .get_all_by_source(source)
            .await
            .map_err(|err| select_error("GetGroupsBySource", &err))
    }

    /// Port of `App.GetGroupsByUserId` (app/group.go:92). Of the options the store reads only
    /// `FilterAllowReference`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn get_groups_by_user_id(
        &self,
        user_id: &str,
        filter_allow_reference: bool,
    ) -> AppResult<Vec<Group>> {
        self.store()
            .group()
            .get_by_user(user_id, filter_allow_reference)
            .await
            .map_err(|err| select_error("GetGroupsByUserId", &err))
    }

    /// Port of `App.GetGroupMemberUsersPage` (app/group.go:320) over
    /// `GetGroupMemberUsersSortedPage` with no view restrictions and `ShowUsername`: the page,
    /// then the member count (read and failed on, though the plugin API drops it), then
    /// `sanitizeProfiles(members, false)` — as a **non-admin**, so the e-mail and full name follow
    /// the privacy settings.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, page, per_page))]
    pub async fn get_group_member_users_page(
        &self,
        group_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<(Vec<User>, i64)> {
        let mut members = self
            .store()
            .group()
            .get_member_users_page(group_id, page, per_page)
            .await
            .map_err(|err| select_error("GetGroupMemberUsersPage", &err))?;
        let count = self
            .store()
            .group()
            .get_member_count(group_id)
            .await
            .map_err(|err| select_error("GetGroupMemberCount", &err))?;
        for member in &mut members {
            self.sanitize_profile(member, false);
        }
        Ok((members, count))
    }

    /// Port of `App.UpsertGroupMember` (app/group.go:333); see the module note. A missing group
    /// is a plain store error, so 500 `app.update_error`.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, user_id = %user_id))]
    pub async fn upsert_group_member(
        &self,
        group_id: &str,
        user_id: &str,
    ) -> AppResult<GroupMember> {
        let member = self
            .store()
            .group()
            .upsert_members(group_id, &[user_id.to_owned()])
            .await
            .map_err(|err| match err {
                StoreError::Invalid { app_error, .. } => app_error,
                StoreError::InvalidInput { .. } => AppError::boxed(
                    "UpsertGroupMember",
                    "app.group.uniqueness_error",
                    None,
                    String::new(),
                    400,
                ),
                StoreError::NotFound { criteria, .. } => {
                    let mut params = std::collections::HashMap::new();
                    params.insert("Username".to_owned(), serde_json::Value::from(criteria));
                    AppError::boxed(
                        "UpsertGroupMember",
                        "app.group.user_not_found",
                        Some(params),
                        String::new(),
                        400,
                    )
                }
                other => {
                    tracing::error!(error = %other, "group member upsert failed");
                    AppError::boxed(
                        "UpsertGroupMember",
                        "app.update_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?
            .into_iter()
            .next()
            .ok_or_else(|| {
                AppError::boxed(
                    "UpsertGroupMember",
                    "app.update_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        self.publish_group_member_event(WEBSOCKET_EVENT_GROUP_MEMBER_ADD, &member)
            .await?;
        Ok(member)
    }

    /// Port of `App.DeleteGroupMember` (app/group.go:358): 404 `app.group.no_rows` when the user
    /// has no live membership, 500 `app.update_error` otherwise; see the module note.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, user_id = %user_id))]
    pub async fn delete_group_member(
        &self,
        group_id: &str,
        user_id: &str,
    ) -> AppResult<GroupMember> {
        let failed = |status: i32| {
            let id = if status == 404 {
                "app.group.no_rows"
            } else {
                "app.update_error"
            };
            AppError::boxed("DeleteGroupMember", id, None, String::new(), status)
        };
        let member = self
            .store()
            .group()
            .delete_members(group_id, &[user_id.to_owned()])
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    failed(404)
                } else {
                    tracing::error!(error = %err, "group member delete failed");
                    failed(500)
                }
            })?
            .into_iter()
            .next()
            .ok_or_else(|| failed(500))?;
        self.publish_group_member_event(WEBSOCKET_EVENT_GROUP_MEMBER_DELETE, &member)
            .await?;
        Ok(member)
    }

    /// Port of `App.GetGroupSyncables` (app/group.go:487): the live links of one type.
    #[tracing::instrument(skip_all, fields(group_id = %group_id))]
    pub async fn get_group_syncables(
        &self,
        group_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> AppResult<Vec<GroupSyncable>> {
        self.store()
            .group()
            .get_all_group_syncables_by_group_id(group_id, syncable_type)
            .await
            .map_err(|err| select_error("GetGroupSyncables", &err))
    }

    /// Port of `App.GetGroups` (app/group.go:660): the store's list, then, with
    /// `IncludeMemberIDs`, each group's live members' ids — a query per group, whose failure is
    /// `app.member_count` with `Where` `GetGroup`, Go's copy-paste.
    #[tracing::instrument(skip_all, fields(page, per_page))]
    pub async fn get_groups(
        &self,
        page: i64,
        per_page: i64,
        opts: &GroupSearchOpts,
        view_restrictions: Option<&ViewUsersRestrictions>,
    ) -> AppResult<Vec<Group>> {
        let mut groups = self
            .store()
            .group()
            .get_groups(page, per_page, opts, view_restrictions)
            .await
            .map_err(|err| select_error("GetGroups", &err))?;
        if opts.include_member_ids {
            for group in &mut groups {
                let users = self
                    .store()
                    .group()
                    .get_member_users(&group.id)
                    .await
                    .map_err(|err| {
                        tracing::error!(error = %err, "group member ids failed");
                        AppError::boxed("GetGroup", "app.member_count", None, String::new(), 500)
                    })?;
                // `append` to a nil slice: a group with no member keeps its nil.
                if !users.is_empty() {
                    group
                        .member_ids
                        .get_or_insert_with(Vec::new)
                        .extend(users.into_iter().map(|u| u.id));
                }
            }
        }
        Ok(groups)
    }

    /// Port of `App.CreateDefaultMemberships` (app/syncables.go:128): the team memberships the
    /// auto-add links call for, then the channel ones; the first failure ends it. The error is
    /// the multi-error's text, which the plugin API wraps and drops.
    #[tracing::instrument(skip_all)]
    pub async fn create_default_memberships(
        &self,
        params: &CreateDefaultMembershipParams,
        hook_ctx: &HookContext,
    ) -> Result<(), String> {
        self.create_default_team_memberships(params, hook_ctx)
            .await?;
        self.create_default_channel_memberships(params, hook_ctx)
            .await
    }

    /// Port of `App.DeleteGroupConstrainedMemberships` (app/syncables.go:144): over **every**
    /// group-constrained channel, then every such team, the memberships of users in none of the
    /// allowed groups are removed.
    #[tracing::instrument(skip_all)]
    pub async fn delete_group_constrained_memberships(
        &self,
        hook_ctx: &HookContext,
    ) -> Result<(), String> {
        self.delete_group_constrained_channel_memberships(None, hook_ctx)
            .await?;
        self.delete_group_constrained_team_memberships(None, hook_ctx)
            .await
    }
}
