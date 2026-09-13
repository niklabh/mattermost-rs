//! Port of the two reads from `channels/app/group.go` the routes this server answers need.
//!
//! # Why this file exists at all
//!
//! Every one of the twenty routes in `api4/group.go` opens with `requireLicense` and is answered
//! here as a 501 (see `mm_api::groups`), so nothing in that file reaches a group table. The
//! exceptions are `channelMembersMinusGroupMembers` (api4/channel.go:2881) and
//! `teamMembersMinusGroupMembers` (api4/team.go:2222), which live in the *channel* and *team*
//! files, have **no licence gate**, and answer a group question on an unlicensed server. They are
//! the only callers of everything below.

use mm_model::group::Group;
use mm_model::user::UserWithGroups;
use mm_model::utils::{AppError, AppResult};
use mm_store::group_store::GroupStore;

use crate::App;

impl App {
    /// Port of `App.ChannelMembersMinusGroupMembers` (app/group.go:755).
    ///
    /// The members of `channel_id` who are in **none** of `group_ids` — "who would be removed if
    /// this channel were group-constrained to these groups" — one page of them, plus the total.
    ///
    /// # The group hydration is a second query, and an id it does not resolve is dropped
    ///
    /// The page query returns each user's group ids as one `string_agg` column; this then fetches
    /// the distinct set in one `GetByIDs` and maps them back. Go's inner loop is
    /// `if group, ok := groupMap[groupID]; ok` — so a group id that `GetByIDs` did not return
    /// (which cannot happen through a foreign key, but the code allows it) is **skipped**, not
    /// nil-appended. Reproduced with `filter_map`.
    ///
    /// # `Groups` is always a list, never null
    ///
    /// `user.Groups = []*model.Group{}` runs before the inner loop for every user
    /// (app/group.go:791), so a member of no group is `"groups": []` on the wire and the store's
    /// nil is never what a client sees. That assignment is the whole reason `UserWithGroups.groups`
    /// is an `Option` at all — the type can express Go's nil, and this function is where it stops
    /// being one.
    ///
    /// # `SanitizeProfile(&u.User, false)` — `false`, not `true`
    ///
    /// This is a System Console route reached with `sysconsole_read_user_management_channels`, and
    /// it still sanitises as a **non-admin**: `ShowEmailAddress` and `ShowFullName` decide whether
    /// the email and the names survive, exactly as they do for any other caller. A port that
    /// passed `true` would leak both on a privacy-configured server.
    #[tracing::instrument(skip(self, group_ids), fields(channel_id = %channel_id, groups = group_ids.len(), users, total))]
    pub async fn channel_members_minus_group_members(
        &self,
        channel_id: &str,
        group_ids: &[String],
        page: i64,
        per_page: i64,
    ) -> AppResult<(Vec<UserWithGroups>, i64)> {
        let mut users = self
            .store
            .group()
            .channel_members_minus_group_members(channel_id, group_ids, page, per_page)
            .await
            .map_err(|source| {
                AppError::boxed(
                    "ChannelMembersMinusGroupMembers",
                    "app.select_error",
                    None,
                    source.to_string(),
                    500,
                )
            })?;

        self.sanitize_and_hydrate_groups(&mut users).await?;

        let total = self
            .store
            .group()
            .count_channel_members_minus_group_members(channel_id, group_ids)
            .await
            .map_err(|source| {
                AppError::boxed(
                    "ChannelMembersMinusGroupMembers",
                    "app.select_error",
                    None,
                    source.to_string(),
                    500,
                )
            })?;

        tracing::Span::current().record("users", users.len());
        tracing::Span::current().record("total", total);
        Ok((users, total))
    }

    /// Port of `App.TeamMembersMinusGroupMembers` (app/group.go:687).
    ///
    /// The members of `team_id` who are in **none** of `group_ids`, one page of them, plus the
    /// total. Character-for-character the same function as
    /// [`App::channel_members_minus_group_members`] with the store pair swapped — including the
    /// `SanitizeProfile(..., false)`, the `Groups = []` assignment and the dropped-id `filter_map`,
    /// all of which are documented there. Go duplicates the body too; the shared half lives in
    /// [`App::sanitize_and_hydrate_groups`].
    ///
    /// **The `where` on a store failure is `TeamMembersMinusGroupMembers`**, not the channel
    /// name — the only thing a client can tell apart when the database is down.
    #[tracing::instrument(skip(self, group_ids), fields(team_id = %team_id, groups = group_ids.len(), users, total))]
    pub async fn team_members_minus_group_members(
        &self,
        team_id: &str,
        group_ids: &[String],
        page: i64,
        per_page: i64,
    ) -> AppResult<(Vec<UserWithGroups>, i64)> {
        let mut users = self
            .store
            .group()
            .team_members_minus_group_members(team_id, group_ids, page, per_page)
            .await
            .map_err(|source| {
                AppError::boxed(
                    "TeamMembersMinusGroupMembers",
                    "app.select_error",
                    None,
                    source.to_string(),
                    500,
                )
            })?;

        self.sanitize_and_hydrate_groups(&mut users).await?;

        let total = self
            .store
            .group()
            .count_team_members_minus_group_members(team_id, group_ids)
            .await
            .map_err(|source| {
                AppError::boxed(
                    "TeamMembersMinusGroupMembers",
                    "app.select_error",
                    None,
                    source.to_string(),
                    500,
                )
            })?;

        tracing::Span::current().record("users", users.len());
        tracing::Span::current().record("total", total);
        Ok((users, total))
    }

    /// The half of `TeamMembersMinusGroupMembers` and `ChannelMembersMinusGroupMembers` that Go
    /// writes out twice: sanitise every profile as a non-admin, then replace each user's
    /// `string_agg`ed group ids with the group rows themselves.
    ///
    /// Every decision here is documented on [`App::channel_members_minus_group_members`]; this is
    /// where they are actually taken.
    async fn sanitize_and_hydrate_groups(&self, users: &mut [UserWithGroups]) -> AppResult {
        for user in users.iter_mut() {
            self.sanitize_profile(&mut user.user, false);
        }

        // The distinct group ids across every user on this page, in Go's `map`-then-slice shape.
        // Order does not reach the wire — the per-user loop below re-orders by each user's own
        // `GetGroupIDs` — so a `BTreeSet` buys determinism for free.
        let wanted: Vec<String> = users
            .iter()
            .filter_map(UserWithGroups::get_group_ids)
            .flatten()
            .map(str::to_owned)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        let groups = self.get_groups_by_ids(&wanted).await?;
        let by_id: std::collections::HashMap<&str, &Group> =
            groups.iter().map(|g| (g.id.as_str(), g)).collect();

        for user in users.iter_mut() {
            let hydrated = user
                .get_group_ids()
                .unwrap_or_default()
                .into_iter()
                // A clone per group is the answer's own storage, not a borrow-checker dodge:
                // `groups` is local and each user's list is an independent copy, as it is in Go.
                .filter_map(|id| by_id.get(id).map(|g| (*g).clone()))
                .collect();
            user.groups = Some(hydrated);
        }

        Ok(())
    }

    /// Port of `App.GetGroupsByIDs` (app/group.go:740).
    ///
    /// **`app.select_error` at 500**, the generic store error id, not one of its own.
    #[tracing::instrument(skip(self, group_ids), fields(asked = group_ids.len()))]
    pub async fn get_groups_by_ids(&self, group_ids: &[String]) -> AppResult<Vec<Group>> {
        self.store
            .group()
            .get_groups_by_ids(group_ids)
            .await
            .map_err(|source| {
                AppError::boxed(
                    "GetGroupsByIDs",
                    "app.select_error",
                    None,
                    source.to_string(),
                    500,
                )
            })
    }
}

// ---------------------------------------------------------------------------------------------
// The seven CRUD and membership writes of `api4/group.go`, ported 2026-09-13 against the licensed
// pair ([D-360]). Every function here sits behind `requireLicense` in its handler, so none is
// reachable on an unlicensed server; the licence question itself is the handler's, not these.
// ---------------------------------------------------------------------------------------------

use mm_model::group::{GroupSource, GroupWithUserIds};
use mm_model::group_member::GroupMember;
use mm_model::license::minimum_professional_license;
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_GROUP_MEMBER_ADD, WEBSOCKET_EVENT_GROUP_MEMBER_DELETE,
    WEBSOCKET_EVENT_RECEIVED_GROUP, WebSocketEvent,
};
use mm_store::{StoreError, UserStore};

impl App {
    /// Port of `licensedAndConfiguredForGroupBySource` (api4/group.go:1566).
    ///
    /// Returns the error with a **blank `where`**, exactly as Go does; every caller sets its own
    /// (`Api4.createGroup`, `Api4.patchGroup`, …) before answering. Five refusals, in order:
    ///
    /// | condition | id | status |
    /// |---|---|---|
    /// | no licence | `api.license_error` | **403** — not the 501 `requireLicense` gives for the same fact |
    /// | `ldap` source without `Features.LDAPGroups` | `api.ldap_groups.license_error` | 403 |
    /// | a `plugin_` source without `Features.LDAPGroups` | `api.ldap_groups.license_error` | 403 |
    /// | `custom` below the Professional tier | `api.custom_groups.license_error` | 400 |
    /// | `custom` with `ServiceSettings.EnableCustomGroups` off | `api.custom_groups.feature_disabled` | 400 |
    ///
    /// The first arm is unreachable from api4 — `requireLicense` answered already — but it is
    /// the arm a caller with a stale licence reads, and it is kept because the status differs.
    /// `*lic.Features.LDAPGroups` is dereferenced unguarded in Go; `SetDefaults` ran at load, so
    /// it is never nil there and never `None` here.
    #[tracing::instrument(skip_all, fields(source = %source.as_str()))]
    pub async fn licensed_and_configured_for_group_by_source(
        &self,
        source: &GroupSource,
    ) -> AppResult<()> {
        let Some(license) = self.license().await? else {
            return Err(AppError::boxed(
                "",
                "api.license_error",
                None,
                String::new(),
                403,
            ));
        };
        let ldap_groups = license
            .features
            .as_ref()
            .and_then(|f| f.ldap_groups)
            .unwrap_or(false);

        if source.as_str() == GroupSource::LDAP && !ldap_groups {
            return Err(ldap_groups_license_error());
        }
        if source.as_str().starts_with(GroupSource::PLUGIN_PREFIX) && !ldap_groups {
            return Err(ldap_groups_license_error());
        }
        if source.as_str() == GroupSource::CUSTOM && !minimum_professional_license(Some(&license)) {
            return Err(AppError::boxed(
                "",
                "api.custom_groups.license_error",
                None,
                String::new(),
                400,
            ));
        }
        if source.as_str() == GroupSource::CUSTOM && !self.config().enable_custom_groups {
            return Err(AppError::boxed(
                "",
                "api.custom_groups.feature_disabled",
                None,
                String::new(),
                400,
            ));
        }
        Ok(())
    }

    /// Port of `App.GetGroup` (app/group.go:16) with `opts == nil`: the row and nothing computed.
    ///
    /// `IncludeMemberIDs` and `IncludeMemberCount` have no caller among the seven writes, so the
    /// two extra queries they would trigger are not ported here.
    #[tracing::instrument(skip_all, fields(group_id = %group_id))]
    pub async fn get_group(&self, group_id: &str) -> AppResult<Group> {
        self.store().group().get(group_id).await.map_err(|err| {
            let (id, status) = match err {
                StoreError::NotFound { .. } => ("app.group.no_rows", 404),
                _ => {
                    tracing::error!(error = %err, "group lookup failed");
                    ("app.select_error", 500)
                }
            };
            AppError::boxed("GetGroup", id, None, String::new(), status)
        })
    }

    /// Port of `App.GetGroupByName` (app/group.go:53). Only `FilterAllowReference` of the
    /// options is read by the store.
    #[tracing::instrument(skip_all, fields(name = %name))]
    pub async fn get_group_by_name(
        &self,
        name: &str,
        filter_allow_reference: bool,
    ) -> AppResult<Group> {
        self.store()
            .group()
            .get_by_name(name, filter_allow_reference)
            .await
            .map_err(|err| {
                let (id, status) = match err {
                    StoreError::NotFound { .. } => ("app.group.no_rows", 404),
                    _ => {
                        tracing::error!(error = %err, "group-by-name lookup failed");
                        ("app.select_error", 500)
                    }
                };
                AppError::boxed("GetGroupByName", id, None, String::new(), status)
            })
    }

    /// Port of `App.GetGroupsByNames` (app/group.go:101).
    #[tracing::instrument(skip_all, fields(names = names.len()))]
    pub async fn get_groups_by_names(
        &self,
        names: &[String],
        filter_allow_reference: bool,
    ) -> AppResult<Vec<Group>> {
        self.store()
            .group()
            .get_by_names(names, filter_allow_reference)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "groups-by-names lookup failed");
                AppError::boxed(
                    "GetGroupsByNames",
                    "app.select_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `App.isUniqueToUsernames` (app/group.go:132): a group may not take a name any
    /// user has. An empty name is not checked; a store failure other than "no such user" is a
    /// 500 whose id is `model.NoTranslation` — the literal `<untranslated>`.
    async fn is_unique_to_usernames(&self, name: &str) -> AppResult<()> {
        if name.is_empty() {
            return Ok(());
        }
        match self.store().user().get_by_username(name).await {
            Ok(_) => {
                let mut params = std::collections::HashMap::new();
                params.insert("Username".to_owned(), serde_json::Value::from(name));
                Err(AppError::boxed(
                    "isUniqueToUsernames",
                    "app.group.username_conflict",
                    Some(params),
                    String::new(),
                    400,
                ))
            }
            Err(StoreError::NotFound { .. }) => Ok(()),
            Err(err) => {
                tracing::error!(error = %err, "username uniqueness check failed");
                Err(AppError::boxed(
                    "isUniqueToUsernames",
                    mm_model::utils::NO_TRANSLATION,
                    None,
                    String::new(),
                    500,
                ))
            }
        }
    }

    /// Port of `App.CreateGroupWithUserIds` (app/group.go:147).
    ///
    /// The store's four failures map to three ids — and `ErrNotFound`, which the store raises
    /// for a member id naming no active user, is **not** in Go's switch: it falls to the default
    /// arm and is a **500** `app.insert_error`, not the 400 `UpsertGroupMembers` gives the same
    /// fact. Reproduced. On success the member count is re-read and a `received_group` event is
    /// published to everyone, its payload the group as a JSON **string**.
    #[tracing::instrument(skip_all, fields(name = group.group.get_name()))]
    pub async fn create_group_with_user_ids(&self, group: GroupWithUserIds) -> AppResult<Group> {
        self.is_unique_to_usernames(group.group.get_name())
            .await
            .map_err(|mut err| {
                err.where_ = "CreateGroupWithUserIds".to_owned();
                err
            })?;

        let mut created = self
            .store()
            .group()
            .create_with_user_ids(group)
            .await
            .map_err(|err| match err {
                StoreError::Invalid { app_error, .. } => app_error,
                StoreError::InvalidInput { .. } => AppError::boxed(
                    "CreateGroupWithUserIds",
                    "app.group.id.app_error",
                    None,
                    String::new(),
                    400,
                ),
                StoreError::Conflict { .. } => AppError::boxed(
                    "CreateGroupWithUserIds",
                    "app.custom_group.unique_name",
                    None,
                    String::new(),
                    400,
                ),
                other => {
                    tracing::error!(error = %other, "group create failed");
                    AppError::boxed(
                        "CreateGroupWithUserIds",
                        "app.insert_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        let count = self
            .member_count_or_id_error(&created.id, "CreateGroupWithUserIds")
            .await?;
        created.member_count = Some(count);
        self.publish_received_group(&created).await?;
        Ok(created)
    }

    /// Port of `App.UpdateGroup` (app/group.go:186). The duplicate-name arm names
    /// **`CreateGroup`** as its `where` — copied from its neighbour in Go and kept, since
    /// `where` is not on the wire.
    #[tracing::instrument(skip_all, fields(group_id = %group.id))]
    pub async fn update_group(&self, group: Group) -> AppResult<Group> {
        self.is_unique_to_usernames(group.get_name())
            .await
            .map_err(|mut err| {
                err.where_ = "UpdateGroup".to_owned();
                err
            })?;

        let mut updated = self
            .store()
            .group()
            .update(group)
            .await
            .map_err(|err| match err {
                StoreError::Invalid { app_error, .. } => app_error,
                StoreError::NotFound { .. } => {
                    AppError::boxed("UpdateGroup", "app.group.no_rows", None, String::new(), 404)
                }
                StoreError::Conflict { .. } => AppError::boxed(
                    "CreateGroup",
                    "app.custom_group.unique_name",
                    None,
                    String::new(),
                    400,
                ),
                other => {
                    tracing::error!(error = %other, "group update failed");
                    AppError::boxed("UpdateGroup", "app.select_error", None, String::new(), 500)
                }
            })?;

        let count = self
            .member_count_or_id_error(&updated.id, "UpdateGroup")
            .await?;
        updated.member_count = Some(count);
        self.publish_received_group(&updated).await?;
        Ok(updated)
    }

    /// Port of `App.DeleteGroup` (app/group.go:227). A group already deleted is `no_rows` at
    /// 404, because the store's select carries `DeleteAt = 0`.
    #[tracing::instrument(skip_all, fields(group_id = %group_id))]
    pub async fn delete_group(&self, group_id: &str) -> AppResult<Group> {
        let mut deleted = self
            .store()
            .group()
            .delete(group_id)
            .await
            .map_err(|err| match err {
                StoreError::NotFound { .. } => {
                    AppError::boxed("DeleteGroup", "app.group.no_rows", None, String::new(), 404)
                }
                other => {
                    tracing::error!(error = %other, "group delete failed");
                    AppError::boxed("DeleteGroup", "app.update_error", None, String::new(), 500)
                }
            })?;
        let count = self
            .member_count_or_id_error(group_id, "DeleteGroup")
            .await?;
        deleted.member_count = Some(count);
        self.publish_received_group(&deleted).await?;
        Ok(deleted)
    }

    /// Port of `App.RestoreGroup` (app/group.go:258) — the mirror of [`App::delete_group`].
    #[tracing::instrument(skip_all, fields(group_id = %group_id))]
    pub async fn restore_group(&self, group_id: &str) -> AppResult<Group> {
        let mut restored =
            self.store()
                .group()
                .restore(group_id)
                .await
                .map_err(|err| match err {
                    StoreError::NotFound { .. } => AppError::boxed(
                        "RestoreGroup",
                        "app.group.no_rows",
                        None,
                        String::new(),
                        404,
                    ),
                    other => {
                        tracing::error!(error = %other, "group restore failed");
                        AppError::boxed(
                            "RestoreGroup",
                            "app.update_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                })?;
        let count = self
            .member_count_or_id_error(group_id, "RestoreGroup")
            .await?;
        restored.member_count = Some(count);
        self.publish_received_group(&restored).await?;
        Ok(restored)
    }

    /// Port of `App.UpsertGroupMembers` (app/group.go:824). A member id naming no active user
    /// is `app.group.user_not_found` at 400 with the id under the `Username` parameter — the
    /// parameter name is Go's. One `group_member_add` event per member, each addressed to that
    /// member alone.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, users = user_ids.len()))]
    pub async fn upsert_group_members(
        &self,
        group_id: &str,
        user_ids: &[String],
    ) -> AppResult<Vec<GroupMember>> {
        let members = self
            .store()
            .group()
            .upsert_members(group_id, user_ids)
            .await
            .map_err(|err| {
                group_member_write_error(err, "UpsertGroupMembers", "app.update_error")
            })?;
        for member in &members {
            self.publish_group_member_event(WEBSOCKET_EVENT_GROUP_MEMBER_ADD, member)
                .await?;
        }
        Ok(members)
    }

    /// Port of `App.DeleteGroupMembers` (app/group.go:851). Its `where` is the singular
    /// **`DeleteGroupMember`** in Go, on every arm.
    #[tracing::instrument(skip_all, fields(group_id = %group_id, users = user_ids.len()))]
    pub async fn delete_group_members(
        &self,
        group_id: &str,
        user_ids: &[String],
    ) -> AppResult<Vec<GroupMember>> {
        let members = self
            .store()
            .group()
            .delete_members(group_id, user_ids)
            .await
            .map_err(|err| {
                group_member_write_error(err, "DeleteGroupMember", "app.update_error")
            })?;
        for member in &members {
            self.publish_group_member_event(WEBSOCKET_EVENT_GROUP_MEMBER_DELETE, member)
                .await?;
        }
        Ok(members)
    }

    /// `GetMemberCount` as the four group writes call it: a failure is `app.group.id.app_error`
    /// at **400** — Go's choice of id and status for what is a store fault, kept.
    async fn member_count_or_id_error(&self, group_id: &str, where_: &str) -> AppResult<i64> {
        self.store()
            .group()
            .get_member_count(group_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "group member count failed");
                AppError::boxed(where_, "app.group.id.app_error", None, String::new(), 400)
            })
    }

    /// The `received_group` event the four group writes publish: no team, channel or user in
    /// the broadcast — every connection gets it — and the group marshalled as a JSON **string**
    /// under `group`, not embedded as an object.
    async fn publish_received_group(&self, group: &Group) -> AppResult<()> {
        let json = serde_json::to_string(group).map_err(|err| {
            tracing::error!(error = %err, "group does not serialise");
            AppError::boxed("UpdateGroup", "api.marshal_error", None, String::new(), 500)
        })?;
        let mut event = WebSocketEvent::new(WEBSOCKET_EVENT_RECEIVED_GROUP, "", "", "", None, "");
        event.add("group", serde_json::Value::String(json));
        self.publish(event).await;
        Ok(())
    }

    /// Port of `App.publishGroupMemberEvent` (app/group.go:879): addressed to the member's own
    /// user id, payload the membership row as a JSON string under `group_member`.
    async fn publish_group_member_event(
        &self,
        event_name: &str,
        member: &GroupMember,
    ) -> AppResult<()> {
        let json = serde_json::to_string(member).map_err(|err| {
            tracing::error!(error = %err, "group member does not serialise");
            AppError::boxed(
                "publishGroupMemberEvent",
                "api.marshal_error",
                None,
                String::new(),
                500,
            )
        })?;
        let mut event = WebSocketEvent::new(event_name, "", "", &member.user_id, None, "");
        event.add("group_member", serde_json::Value::String(json));
        self.publish(event).await;
        Ok(())
    }
}

/// `api.ldap_groups.license_error` at 403, from `licensedAndConfiguredForGroupBySource` —
/// the same id `getLdapGroups` answers at **501**, two files away.
fn ldap_groups_license_error() -> Box<AppError> {
    AppError::boxed(
        "",
        "api.ldap_groups.license_error",
        None,
        String::new(),
        403,
    )
}

/// The shared switch of `UpsertGroupMembers` and `DeleteGroupMembers`: an `AppError` passes
/// through, `ErrInvalidInput` is `uniqueness_error`, `ErrNotFound` is `user_not_found` with the
/// missing id under `Username`, and anything else is the 500 the caller names.
fn group_member_write_error(err: StoreError, where_: &str, fallback_id: &str) -> Box<AppError> {
    match err {
        StoreError::Invalid { app_error, .. } => app_error,
        StoreError::InvalidInput { .. } => AppError::boxed(
            where_,
            "app.group.uniqueness_error",
            None,
            String::new(),
            400,
        ),
        StoreError::NotFound { criteria, .. } => {
            let mut params = std::collections::HashMap::new();
            params.insert("Username".to_owned(), serde_json::Value::from(criteria));
            AppError::boxed(
                where_,
                "app.group.user_not_found",
                Some(params),
                String::new(),
                400,
            )
        }
        other => {
            tracing::error!(error = %other, "group membership write failed");
            AppError::boxed(where_, fallback_id, None, String::new(), 500)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The group half of the mention engine: which groups an `@name` in a channel can resolve to.
// ---------------------------------------------------------------------------------------------

use std::collections::BTreeMap;

use mm_model::channel::Channel;
use mm_model::group::GroupSearchOpts;
use mm_model::team::Team;

impl App {
    /// Port of `App.getGroupsAllowedForReferenceInChannel` (app/notification.go:1498): the
    /// groups a mention in `channel` may refer to, keyed by group id.
    ///
    /// Two shapes, decided by `IsGroupConstrained` on the channel and then the team:
    ///
    /// - **Neither is constrained**: every live group with `AllowReference`, from
    ///   [`mm_store::GroupStore::get_groups`] with `FilterAllowReference` and
    ///   `IncludeMemberCount` set and **no page** (`0, 0`).
    /// - **Either is constrained**: the groups linked to the channel — or, only when the channel
    ///   itself is not constrained, to the team — through
    ///   [`mm_store::GroupStore::get_groups_by_channel`] / `get_groups_by_team`, **plus every
    ///   custom group** (`Source: custom`) from the same `get_groups` call. A channel-constrained
    ///   channel in a team-constrained team reads the channel's links only.
    ///
    /// In every branch a group whose `Name` is nil is dropped — an LDAP group that was never given
    /// a mention name cannot be mentioned — and the `GroupWithSchemeAdmin` wrapper is discarded,
    /// so the map holds plain `Group`s with their `member_count` set.
    ///
    /// `team` is Go's `*model.Team`, which `countThreadMentions` (app/post.go:2505) leaves nil
    /// for a channel with no team. Go's `team != nil && team.IsGroupConstrained()` is the
    /// `Option` here.
    ///
    /// # The error is not one of Go's `AppError`s
    ///
    /// Go returns a plain `error` ("unable to get groups", "unable to get custom groups") and
    /// every caller wraps it in its own id — `countThreadMentions` uses
    /// `app.channel.count_posts_since.app_error` at 500 (post.go:2547). This answers
    /// `app.select_error` at 500 with the Go message as detail, the way the other store failures
    /// in this module do; a caller that must match Go's id on the wire re-wraps it.
    #[tracing::instrument(skip(self, channel, team), fields(channel_id = %channel.id, groups))]
    pub async fn get_groups_allowed_for_reference_in_channel(
        &self,
        channel: &Channel,
        team: Option<&Team>,
    ) -> AppResult<BTreeMap<String, Group>> {
        let mut groups_map = BTreeMap::new();
        let mut opts = GroupSearchOpts {
            filter_allow_reference: true,
            include_member_count: true,
            ..GroupSearchOpts::default()
        };
        let store = self.store.group();
        let failed = |detail: &str, source: StoreError| {
            AppError::boxed(
                "getGroupsAllowedForReferenceInChannel",
                "app.select_error",
                None,
                format!("{detail}: {source}"),
                500,
            )
        };

        if channel.is_group_constrained() || team.is_some_and(Team::is_group_constrained) {
            let linked = if channel.is_group_constrained() {
                store.get_groups_by_channel(&channel.id, &opts).await
            } else {
                // `team` is `Some` here: the `||` above fell through to the team's own flag.
                let team_id = team.map(|t| t.id.as_str()).unwrap_or_default();
                store.get_groups_by_team(team_id, &opts).await
            }
            .map_err(|source| failed("unable to get groups", source))?;
            for group in linked {
                if group.group.name.is_some() {
                    // The key is the map's own copy of the id; the value keeps the other.
                    groups_map.insert(group.group.id.clone(), group.group);
                }
            }

            opts.source = GroupSource::from(GroupSource::CUSTOM);
            let custom = store
                .get_groups(0, 0, &opts, None)
                .await
                .map_err(|source| failed("unable to get custom groups", source))?;
            for group in custom {
                if group.name.is_some() {
                    // As above: the key is the map's copy of the id.
                    groups_map.insert(group.id.clone(), group);
                }
            }
            tracing::Span::current().record("groups", groups_map.len());
            return Ok(groups_map);
        }

        let groups = store
            .get_groups(0, 0, &opts, None)
            .await
            .map_err(|source| failed("unable to get groups", source))?;
        for group in groups {
            if group.name.is_some() {
                // As above: the key is the map's copy of the id.
                groups_map.insert(group.id.clone(), group);
            }
        }
        tracing::Span::current().record("groups", groups_map.len());
        Ok(groups_map)
    }
}
