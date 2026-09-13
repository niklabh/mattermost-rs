//! Port of the **syncable** half of `SqlGroupStore` (channels/store/sqlstore/group_store.go):
//! the `GroupTeams`/`GroupChannels` rows that link a group to a team or channel, and the four
//! membership queries the sync behind a link reads.
//!
//! A second trait on [`SqlGroupStore`] rather than more methods on [`crate::GroupStore`], because
//! the two halves were ported by two sessions for two route families and the seam between them is
//! this file boundary: everything here serves `linkGroupSyncable`, `patchGroupSyncable` and
//! `unlinkGroupSyncable` (api4/group.go:319, :527, :624) and the `SyncRolesAndMembership` they
//! dispatch. Group CRUD and membership writes live in `group_store.rs`.
//!
//! # One statement per query, branches chosen by bound parameters
//!
//! Go builds `TeamMembersToAdd` and its three siblings with squirrel, adding a `LEFT OUTER JOIN`
//! and two predicates only when `reAddRemovedMembers` is false and a team filter only when one
//! was given. `sqlx::query!` needs the SQL fixed at compile time, so each optional clause is
//! written once and switched by a parameter: `$3 OR (...)` for the join's predicates (a `LEFT
//! JOIN` on a primary key never multiplies rows, so it can stay in the statement unconditionally)
//! and `$2::text IS NULL OR Teams.Id = $2` for the scope. Same answer, compile-time checked.
//!
//! # The two `*MembersToRemove` return pairs, not members
//!
//! Go selects a `TeamMember`'s eight raw columns (no scheme-role join) and its only caller reads
//! `TeamId` and `UserId` to call `RemoveUserFromTeam`. The row shape is never on the wire and
//! never reaches a `ToModel`, so returning the pair is the same information without a half-filled
//! struct whose `roles` would be the raw column.

use mm_model::group_syncable::{
    GroupSyncable, GroupSyncableType, UserChannelIDPair, UserTeamIDPair,
};
use mm_model::utils::get_millis;

use crate::error::StoreError;
use crate::group_store::SqlGroupStore;

/// The syncable methods of Go's `store.GroupStore` (store/store.go:970).
pub trait GroupSyncableStore {
    /// Port of `SqlGroupStore.GetGroupSyncable` (group_store.go:749) — the row whatever its
    /// `DeleteAt`; a miss is `ErrNotFound`.
    fn get_group_syncable(
        &self,
        group_id: &str,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> impl std::future::Future<Output = Result<GroupSyncable, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetAllGroupSyncablesByGroupId` (group_store.go:828): the **live**
    /// (`DeleteAt = 0`) syncables of one type, joined to their team or channel for the display
    /// fields.
    fn get_all_group_syncables_by_group_id(
        &self,
        group_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> impl std::future::Future<Output = Result<Vec<GroupSyncable>, StoreError>> + Send;

    /// Port of `SqlGroupStore.CreateGroupSyncable` (group_store.go:705).
    ///
    /// `IsValid` first, then the timestamps are **reset** — `DeleteAt` to 0, `CreateAt` and
    /// `UpdateAt` to now — whatever the caller passed; then the team or channel must exist, and
    /// its store's `ErrNotFound` is returned as-is (the app layer names it
    /// `store.sql_channel.get.existing.app_error` for both kinds). For a channel the returned
    /// value carries the channel's `TeamId`.
    fn create_group_syncable(
        &self,
        group_syncable: GroupSyncable,
    ) -> impl std::future::Future<Output = Result<GroupSyncable, StoreError>> + Send;

    /// Port of `SqlGroupStore.UpdateGroupSyncable` (group_store.go:895).
    ///
    /// The row must exist (`ErrNotFound`), the value must be valid, and `DeleteAt` may change
    /// only **to zero** — anything else is a bare error the app layer answers 500 to. `CreateAt`
    /// is taken from the stored row, `UpdateAt` is now. A channel syncable re-reads the channel
    /// for its `TeamId`, and a channel that is gone is the channel store's own error, unwrapped.
    fn update_group_syncable(
        &self,
        group_syncable: GroupSyncable,
    ) -> impl std::future::Future<Output = Result<GroupSyncable, StoreError>> + Send;

    /// Port of `SqlGroupStore.DeleteGroupSyncable` (group_store.go:948): a soft delete that
    /// stamps `DeleteAt` and `UpdateAt` with one clock reading. Deleting a deleted row is
    /// `ErrInvalidInput`, which the app layer answers **400** to.
    fn delete_group_syncable(
        &self,
        group_id: &str,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> impl std::future::Future<Output = Result<GroupSyncable, StoreError>> + Send;

    /// Port of `SqlGroupStore.TeamMembersToAdd` (group_store.go:987): every `(user, team)` an
    /// `AutoAdd` syncable says should be a member. With `re_add_removed_members` false, only
    /// users with no `TeamMembers` row at all, and only where the membership or the link is at
    /// least `since` old.
    fn team_members_to_add(
        &self,
        since: i64,
        team_id: Option<&str>,
        re_add_removed_members: bool,
    ) -> impl std::future::Future<Output = Result<Vec<UserTeamIDPair>, StoreError>> + Send;

    /// Port of `SqlGroupStore.ChannelMembersToAdd` (group_store.go:1023). The channel twin
    /// consults `ChannelMemberHistory` rather than `ChannelMembers`, and asks that **both** the
    /// join's `UserId` and its `LeaveTime` be NULL — i.e. that the user never joined, not merely
    /// that they are not a member now.
    fn channel_members_to_add(
        &self,
        since: i64,
        channel_id: Option<&str>,
        re_add_removed_members: bool,
    ) -> impl std::future::Future<Output = Result<Vec<UserChannelIDPair>, StoreError>> + Send;

    /// Port of `SqlGroupStore.TeamMembersToRemove` (group_store.go:1076): members of
    /// group-constrained teams who are in none of the team's groups. Bots are exempt.
    fn team_members_to_remove(
        &self,
        team_id: Option<&str>,
    ) -> impl std::future::Future<Output = Result<Vec<UserTeamIDPair>, StoreError>> + Send;

    /// Port of `SqlGroupStore.ChannelMembersToRemove` (group_store.go:1250). Bots are exempt,
    /// and so is every channel that is not open or private — Go's comment: only those support
    /// group sync.
    fn channel_members_to_remove(
        &self,
        channel_id: Option<&str>,
    ) -> impl std::future::Future<Output = Result<Vec<UserChannelIDPair>, StoreError>> + Send;

    /// Port of `SqlGroupStore.PermittedSyncableAdmins` (group_store.go:1840): the users who are
    /// members of a group linked to the syncable with `SchemeAdmin`, both links live.
    fn permitted_syncable_admins(
        &self,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;

    /// The ids `SqlGroupStore.GetGroupsByTeam` (group_store.go:1433) would return with empty
    /// options: groups whose `GroupTeams` link to `team_id` is live and which are themselves live.
    /// `UpsertGroupSyncable` (app/group.go:414) only asks whether one id is among them.
    fn group_ids_synced_to_team(
        &self,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;
}

/// One `GroupTeams` or `GroupChannels` row — Go's `groupTeam`/`groupChannel` (group_store.go:25),
/// which embed `GroupSyncable` and add the id column.
struct SyncableRow {
    groupid: String,
    syncableid: String,
    autoadd: Option<bool>,
    schemeadmin: Option<bool>,
    createat: Option<i64>,
    updateat: Option<i64>,
    deleteat: Option<i64>,
}

impl SyncableRow {
    /// Port of the copy in `getGroupSyncable` (group_store.go:794). Every nullable column is a
    /// plain `bool`/`int64` in Go's struct, so NULL scans as the zero value.
    fn into_syncable(self, syncable_type: &GroupSyncableType) -> GroupSyncable {
        GroupSyncable {
            group_id: self.groupid,
            syncable_id: self.syncableid,
            auto_add: self.autoadd.unwrap_or_default(),
            scheme_admin: self.schemeadmin.unwrap_or_default(),
            create_at: self.createat.unwrap_or_default(),
            update_at: self.updateat.unwrap_or_default(),
            delete_at: self.deleteat.unwrap_or_default(),
            type_: syncable_type.clone(),
            ..GroupSyncable::default()
        }
    }
}

fn not_found(group_id: &str, syncable_id: &str, syncable_type: &GroupSyncableType) -> StoreError {
    StoreError::NotFound {
        entity: "GroupSyncable",
        criteria: format!(
            "groupId={group_id}, syncableId={syncable_id}, syncableType={syncable_type}"
        ),
    }
}

fn db(context: String) -> impl FnOnce(sqlx::Error) -> StoreError {
    move |source| StoreError::Db { context, source }
}

impl SqlGroupStore {
    /// `getGroupSyncable` (group_store.go:761), the unexported read both the update and the
    /// delete start from. A row is returned whatever its `DeleteAt`.
    async fn fetch_syncable(
        &self,
        group_id: &str,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> Result<Option<SyncableRow>, StoreError> {
        let context = format!(
            "failed to find GroupSyncable with groupId={group_id}, syncableId={syncable_id}, syncableType={syncable_type}"
        );
        match syncable_type.as_str() {
            GroupSyncableType::TEAM => sqlx::query_as!(
                SyncableRow,
                r#"
                SELECT groupid, teamid AS syncableid, autoadd, schemeadmin, createat, updateat, deleteat
                  FROM groupteams
                 WHERE groupid = $1 AND teamid = $2
                "#,
                group_id,
                syncable_id
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(db(context)),
            GroupSyncableType::CHANNEL => sqlx::query_as!(
                SyncableRow,
                r#"
                SELECT groupid, channelid AS syncableid, autoadd, schemeadmin, createat, updateat, deleteat
                  FROM groupchannels
                 WHERE groupid = $1 AND channelid = $2
                "#,
                group_id,
                syncable_id
            )
            .fetch_optional(&self.pool)
            .await
            .map_err(db(context)),
            // Go's switch has no default: `result` stays nil and the function answers
            // `sql.ErrNoRows`, which the public reads render as "not found".
            _ => Ok(None),
        }
    }

    /// `Channels.TeamId` for a syncable channel — what `CreateGroupSyncable` and
    /// `UpdateGroupSyncable` copy onto the returned value. The channel store's own miss.
    async fn channel_team_id(&self, channel_id: &str) -> Result<String, StoreError> {
        let row = sqlx::query!(
            r#"SELECT COALESCE(teamid, '') AS "teamid!" FROM channels WHERE id = $1"#,
            channel_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(db(format!("failed to find channel with id = {channel_id}")))?;
        row.map(|r| r.teamid).ok_or(StoreError::NotFound {
            entity: "Channel",
            criteria: format!("id={channel_id}"),
        })
    }

    /// The team store's `Get`, reduced to existence: `CreateGroupSyncable` looks the team up
    /// only to return its `ErrNotFound`.
    async fn team_exists(&self, team_id: &str) -> Result<(), StoreError> {
        let row = sqlx::query!("SELECT id FROM teams WHERE id = $1", team_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db(format!("failed to find Team with id={team_id}")))?;
        row.map(|_| ()).ok_or(StoreError::NotFound {
            entity: "Team",
            criteria: format!("id={team_id}"),
        })
    }

    /// The `UPDATE` shared by update and delete (group_store.go:929, :976): every mutable
    /// column, keyed by the pair.
    async fn write_syncable(&self, gs: &GroupSyncable) -> Result<(), StoreError> {
        let result = match gs.type_.as_str() {
            GroupSyncableType::TEAM => {
                sqlx::query!(
                    r#"
                UPDATE groupteams
                   SET autoadd = $3, schemeadmin = $4, createat = $5, deleteat = $6, updateat = $7
                 WHERE groupid = $1 AND teamid = $2
                "#,
                    gs.group_id,
                    gs.syncable_id,
                    gs.auto_add,
                    gs.scheme_admin,
                    gs.create_at,
                    gs.delete_at,
                    gs.update_at
                )
                .execute(&self.pool)
                .await
            }
            GroupSyncableType::CHANNEL => {
                sqlx::query!(
                    r#"
                UPDATE groupchannels
                   SET autoadd = $3, schemeadmin = $4, createat = $5, deleteat = $6, updateat = $7
                 WHERE groupid = $1 AND channelid = $2
                "#,
                    gs.group_id,
                    gs.syncable_id,
                    gs.auto_add,
                    gs.scheme_admin,
                    gs.create_at,
                    gs.delete_at,
                    gs.update_at
                )
                .execute(&self.pool)
                .await
            }
            _ => {
                return Err(StoreError::Argument {
                    entity: "GroupSyncable",
                    detail: "invalid GroupSyncableType",
                });
            }
        };
        result
            .map(|_| ())
            .map_err(db("failed to update GroupSyncable".to_owned()))
    }
}

impl GroupSyncableStore for SqlGroupStore {
    #[tracing::instrument(skip(self), fields(found))]
    async fn get_group_syncable(
        &self,
        group_id: &str,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> Result<GroupSyncable, StoreError> {
        let row = self
            .fetch_syncable(group_id, syncable_id, syncable_type)
            .await?;
        tracing::Span::current().record("found", row.is_some());
        row.map(|row| row.into_syncable(syncable_type))
            .ok_or_else(|| not_found(group_id, syncable_id, syncable_type))
    }

    #[tracing::instrument(skip(self), fields(found))]
    async fn get_all_group_syncables_by_group_id(
        &self,
        group_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> Result<Vec<GroupSyncable>, StoreError> {
        let syncables = match syncable_type.as_str() {
            GroupSyncableType::TEAM => sqlx::query!(
                r#"
                SELECT gt.groupid, gt.teamid, gt.autoadd, gt.schemeadmin, gt.createat, gt.updateat,
                       gt.deleteat,
                       COALESCE(t.displayname, '') AS "teamdisplayname!",
                       COALESCE(t.type::text, '')  AS "teamtype!"
                  FROM groupteams gt
                  JOIN teams t ON t.id = gt.teamid
                 WHERE gt.groupid = $1 AND gt.deleteat = 0
                "#,
                group_id
            )
            .fetch_all(&self.pool)
            .await
            .map_err(db(format!(
                "failed to find GroupTeams with groupId={group_id}"
            )))?
            .into_iter()
            .map(|r| GroupSyncable {
                syncable_id: r.teamid,
                group_id: r.groupid,
                auto_add: r.autoadd.unwrap_or_default(),
                create_at: r.createat.unwrap_or_default(),
                delete_at: r.deleteat.unwrap_or_default(),
                update_at: r.updateat.unwrap_or_default(),
                type_: syncable_type.clone(),
                team_display_name: r.teamdisplayname,
                team_type: r.teamtype,
                scheme_admin: r.schemeadmin.unwrap_or_default(),
                ..GroupSyncable::default()
            })
            .collect(),
            GroupSyncableType::CHANNEL => sqlx::query!(
                r#"
                SELECT gc.groupid, gc.channelid, gc.autoadd, gc.schemeadmin, gc.createat,
                       gc.updateat, gc.deleteat,
                       COALESCE(c.displayname, '') AS "channeldisplayname!",
                       COALESCE(t.displayname, '') AS "teamdisplayname!",
                       COALESCE(c.type::text, '')  AS "channeltype!",
                       COALESCE(t.type::text, '')  AS "teamtype!",
                       t.id                        AS "teamid!"
                  FROM groupchannels gc
                  JOIN channels c ON c.id = gc.channelid
                  JOIN teams t ON t.id = c.teamid
                 WHERE gc.groupid = $1 AND gc.deleteat = 0
                "#,
                group_id
            )
            .fetch_all(&self.pool)
            .await
            .map_err(db(format!(
                "failed to find GroupChannels with groupId={group_id}"
            )))?
            .into_iter()
            .map(|r| GroupSyncable {
                syncable_id: r.channelid,
                group_id: r.groupid,
                auto_add: r.autoadd.unwrap_or_default(),
                create_at: r.createat.unwrap_or_default(),
                delete_at: r.deleteat.unwrap_or_default(),
                update_at: r.updateat.unwrap_or_default(),
                type_: syncable_type.clone(),
                channel_display_name: r.channeldisplayname,
                channel_type: r.channeltype,
                team_display_name: r.teamdisplayname,
                team_type: r.teamtype,
                team_id: r.teamid,
                scheme_admin: r.schemeadmin.unwrap_or_default(),
            })
            .collect(),
            // Go's switch has no default and returns the empty slice it started with.
            _ => Vec::new(),
        };
        tracing::Span::current().record("found", syncables.len());
        Ok(syncables)
    }

    #[tracing::instrument(skip_all, fields(group_id = %group_syncable.group_id, syncable_id = %group_syncable.syncable_id, syncable_type = %group_syncable.type_))]
    async fn create_group_syncable(
        &self,
        mut group_syncable: GroupSyncable,
    ) -> Result<GroupSyncable, StoreError> {
        if let Err(app_error) = group_syncable.is_valid() {
            return Err(StoreError::Invalid {
                entity: "GroupSyncable",
                app_error,
            });
        }

        // Reset values that shouldn't be updatable by parameter
        group_syncable.delete_at = 0;
        group_syncable.create_at = get_millis();
        group_syncable.update_at = group_syncable.create_at;

        match group_syncable.type_.as_str() {
            GroupSyncableType::TEAM => {
                self.team_exists(&group_syncable.syncable_id).await?;
                sqlx::query!(
                    r#"
                    INSERT INTO groupteams
                        (groupid, autoadd, schemeadmin, createat, deleteat, updateat, teamid)
                    VALUES ($1, $2, $3, $4, $5, $6, $7)
                    "#,
                    group_syncable.group_id,
                    group_syncable.auto_add,
                    group_syncable.scheme_admin,
                    group_syncable.create_at,
                    group_syncable.delete_at,
                    group_syncable.update_at,
                    group_syncable.syncable_id
                )
                .execute(&self.pool)
                .await
                .map_err(db("unable to insert GroupSyncable".to_owned()))?;
            }
            GroupSyncableType::CHANNEL => {
                let team_id = self.channel_team_id(&group_syncable.syncable_id).await?;
                sqlx::query!(
                    r#"
                    INSERT INTO groupchannels
                        (groupid, autoadd, schemeadmin, createat, deleteat, updateat, channelid)
                    VALUES ($1, $2, $3, $4, $5, $6, $7)
                    "#,
                    group_syncable.group_id,
                    group_syncable.auto_add,
                    group_syncable.scheme_admin,
                    group_syncable.create_at,
                    group_syncable.delete_at,
                    group_syncable.update_at,
                    group_syncable.syncable_id
                )
                .execute(&self.pool)
                .await
                .map_err(db("unable to insert GroupSyncable".to_owned()))?;
                group_syncable.team_id = team_id;
            }
            _ => {
                return Err(StoreError::Argument {
                    entity: "GroupSyncable",
                    detail: "invalid GroupSyncableType",
                });
            }
        }
        Ok(group_syncable)
    }

    #[tracing::instrument(skip_all, fields(group_id = %group_syncable.group_id, syncable_id = %group_syncable.syncable_id, syncable_type = %group_syncable.type_))]
    async fn update_group_syncable(
        &self,
        mut group_syncable: GroupSyncable,
    ) -> Result<GroupSyncable, StoreError> {
        let retrieved = self
            .fetch_syncable(
                &group_syncable.group_id,
                &group_syncable.syncable_id,
                &group_syncable.type_,
            )
            .await?
            .ok_or_else(|| {
                not_found(
                    &group_syncable.group_id,
                    &group_syncable.syncable_id,
                    &group_syncable.type_,
                )
            })?;

        if let Err(app_error) = group_syncable.is_valid() {
            return Err(StoreError::Invalid {
                entity: "GroupSyncable",
                app_error,
            });
        }

        // If updating DeleteAt it can only be to 0
        let retrieved_delete_at = retrieved.deleteat.unwrap_or_default();
        if group_syncable.delete_at != retrieved_delete_at && group_syncable.delete_at != 0 {
            return Err(StoreError::Argument {
                entity: "GroupSyncable",
                detail: "DeleteAt should be 0 when updating",
            });
        }

        // Reset these properties, don't update them based on input
        group_syncable.create_at = retrieved.createat.unwrap_or_default();
        group_syncable.update_at = get_millis();

        // The channel is re-read for its TeamId **before** the write (group_store.go:933), so a
        // channel that is gone means nothing was updated.
        let team_id = if group_syncable.type_.as_str() == GroupSyncableType::CHANNEL {
            Some(self.channel_team_id(&group_syncable.syncable_id).await?)
        } else {
            None
        };
        self.write_syncable(&group_syncable).await?;
        if let Some(team_id) = team_id {
            group_syncable.team_id = team_id;
        }
        Ok(group_syncable)
    }

    #[tracing::instrument(skip(self))]
    async fn delete_group_syncable(
        &self,
        group_id: &str,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> Result<GroupSyncable, StoreError> {
        let mut group_syncable = self
            .fetch_syncable(group_id, syncable_id, syncable_type)
            .await?
            .ok_or_else(|| not_found(group_id, syncable_id, syncable_type))?
            .into_syncable(syncable_type);

        if group_syncable.delete_at != 0 {
            return Err(StoreError::InvalidInput {
                entity: "GroupSyncable",
                field: "<groupId, syncableId, syncableType>",
                value: format!("<{group_id}, {syncable_id}, {syncable_type}>"),
            });
        }

        let time = get_millis();
        group_syncable.delete_at = time;
        group_syncable.update_at = time;
        self.write_syncable(&group_syncable).await?;
        Ok(group_syncable)
    }

    #[tracing::instrument(skip(self), fields(found))]
    async fn team_members_to_add(
        &self,
        since: i64,
        team_id: Option<&str>,
        re_add_removed_members: bool,
    ) -> Result<Vec<UserTeamIDPair>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT gm.userid AS "userid!", gt.teamid AS "teamid!"
              FROM groupmembers gm
              JOIN groupteams gt ON gt.groupid = gm.groupid
              JOIN usergroups ug ON ug.id = gm.groupid
              JOIN teams t ON t.id = gt.teamid
              LEFT OUTER JOIN teammembers tm ON tm.teamid = gt.teamid AND tm.userid = gm.userid
             WHERE ug.deleteat = 0
               AND gt.deleteat = 0
               AND gt.autoadd = true
               AND gm.deleteat = 0
               AND t.deleteat = 0
               AND ($3 OR (tm.userid IS NULL AND (gm.createat >= $1 OR gt.updateat >= $1)))
               AND ($2::text IS NULL OR t.id = $2)
            "#,
            since,
            team_id,
            re_add_removed_members
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db("failed to find UserTeamIDPairs".to_owned()))?;
        tracing::Span::current().record("found", rows.len());
        Ok(rows
            .into_iter()
            .map(|r| UserTeamIDPair {
                user_id: r.userid,
                team_id: r.teamid,
            })
            .collect())
    }

    #[tracing::instrument(skip(self), fields(found))]
    async fn channel_members_to_add(
        &self,
        since: i64,
        channel_id: Option<&str>,
        re_add_removed_members: bool,
    ) -> Result<Vec<UserChannelIDPair>, StoreError> {
        // `ChannelMemberHistory` has no primary key on the pair — a user who joined, left and
        // rejoined has several rows — so unlike the team twin the join *can* multiply rows.
        // Go's `LEFT OUTER JOIN` is inside the `!reAddRemovedMembers` branch only, and there it
        // requires `UserId IS NULL`, so exactly one (null) row survives per pair. Here the join
        // is unconditional and a `DISTINCT` restores the one-row-per-pair Go's other branch has.
        let rows = sqlx::query!(
            r#"
            SELECT DISTINCT gm.userid AS "userid!", gc.channelid AS "channelid!"
              FROM groupmembers gm
              JOIN groupchannels gc ON gc.groupid = gm.groupid
              JOIN usergroups ug ON ug.id = gm.groupid
              JOIN channels c ON c.id = gc.channelid
              LEFT OUTER JOIN channelmemberhistory cmh
                     ON cmh.channelid = gc.channelid AND cmh.userid = gm.userid
             WHERE ug.deleteat = 0
               AND gc.deleteat = 0
               AND gc.autoadd = true
               AND gm.deleteat = 0
               AND c.deleteat = 0
               AND ($3 OR (cmh.userid IS NULL AND cmh.leavetime IS NULL
                           AND (gm.createat >= $1 OR gc.updateat >= $1)))
               AND ($2::text IS NULL OR c.id = $2)
            "#,
            since,
            channel_id,
            re_add_removed_members
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db("failed to find UserChannelIDPairs".to_owned()))?;
        tracing::Span::current().record("found", rows.len());
        Ok(rows
            .into_iter()
            .map(|r| UserChannelIDPair {
                user_id: r.userid,
                channel_id: r.channelid,
            })
            .collect())
    }

    #[tracing::instrument(skip(self), fields(found))]
    async fn team_members_to_remove(
        &self,
        team_id: Option<&str>,
    ) -> Result<Vec<UserTeamIDPair>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT tm.teamid AS "teamid!", tm.userid AS "userid!"
              FROM teammembers tm
              JOIN teams t ON t.id = tm.teamid
              LEFT JOIN bots b ON b.userid = tm.userid
             WHERE tm.deleteat = 0
               AND t.deleteat = 0
               AND t.groupconstrained = true
               AND b.userid IS NULL
               AND (tm.teamid, tm.userid) NOT IN (
                       SELECT t2.id AS teamid, gm.userid
                         FROM teams t2
                         JOIN groupteams gt ON gt.teamid = t2.id
                         JOIN usergroups ug ON ug.id = gt.groupid
                         JOIN groupmembers gm ON gm.groupid = ug.id
                        WHERE t2.groupconstrained = true
                          AND gt.deleteat = 0
                          AND ug.deleteat = 0
                          AND t2.deleteat = 0
                          AND gm.deleteat = 0
                        GROUP BY t2.id, gm.userid)
               AND ($1::text IS NULL OR tm.teamid = $1)
            "#,
            team_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db("failed to find TeamMembers".to_owned()))?;
        tracing::Span::current().record("found", rows.len());
        Ok(rows
            .into_iter()
            .map(|r| UserTeamIDPair {
                user_id: r.userid,
                team_id: r.teamid,
            })
            .collect())
    }

    #[tracing::instrument(skip(self), fields(found))]
    async fn channel_members_to_remove(
        &self,
        channel_id: Option<&str>,
    ) -> Result<Vec<UserChannelIDPair>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT cm.channelid AS "channelid!", cm.userid AS "userid!"
              FROM channelmembers cm
              JOIN channels c ON c.id = cm.channelid
              LEFT JOIN bots b ON b.userid = cm.userid
             WHERE c.deleteat = 0
               AND c.groupconstrained = true
               AND b.userid IS NULL
               AND c.type IN ('O', 'P')
               AND (cm.channelid, cm.userid) NOT IN (
                       SELECT c2.id AS channelid, gm.userid
                         FROM channels c2
                         JOIN groupchannels gc ON gc.channelid = c2.id
                         JOIN usergroups ug ON ug.id = gc.groupid
                         JOIN groupmembers gm ON gm.groupid = ug.id
                        WHERE c2.groupconstrained = true
                          AND gc.deleteat = 0
                          AND ug.deleteat = 0
                          AND c2.deleteat = 0
                          AND gm.deleteat = 0
                        GROUP BY c2.id, gm.userid)
               AND ($1::text IS NULL OR cm.channelid = $1)
            "#,
            channel_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db("failed to find ChannelMembers".to_owned()))?;
        tracing::Span::current().record("found", rows.len());
        Ok(rows
            .into_iter()
            .map(|r| UserChannelIDPair {
                user_id: r.userid,
                channel_id: r.channelid,
            })
            .collect())
    }

    #[tracing::instrument(skip(self), fields(found))]
    async fn permitted_syncable_admins(
        &self,
        syncable_id: &str,
        syncable_type: &GroupSyncableType,
    ) -> Result<Vec<String>, StoreError> {
        // Go interpolates the type into the table and column names (`Group%ss`, `%sId`); the two
        // shapes it can produce are written out.
        let ids = match syncable_type.as_str() {
            GroupSyncableType::TEAM => {
                sqlx::query_scalar!(
                    r#"
                SELECT gm.userid AS "userid!"
                  FROM groupteams gt
                  JOIN groupmembers gm ON gm.groupid = gt.groupid
                       AND gt.schemeadmin = true AND gt.deleteat = 0 AND gm.deleteat = 0
                 WHERE gt.teamid = $1
                "#,
                    syncable_id
                )
                .fetch_all(&self.pool)
                .await
            }
            GroupSyncableType::CHANNEL => {
                sqlx::query_scalar!(
                    r#"
                SELECT gm.userid AS "userid!"
                  FROM groupchannels gc
                  JOIN groupmembers gm ON gm.groupid = gc.groupid
                       AND gc.schemeadmin = true AND gc.deleteat = 0 AND gm.deleteat = 0
                 WHERE gc.channelid = $1
                "#,
                    syncable_id
                )
                .fetch_all(&self.pool)
                .await
            }
            _ => {
                return Err(StoreError::Argument {
                    entity: "GroupSyncable",
                    detail: "invalid GroupSyncableType",
                });
            }
        }
        .map_err(db("failed to find User ids".to_owned()))?;
        tracing::Span::current().record("found", ids.len());
        Ok(ids)
    }

    #[tracing::instrument(skip(self), fields(found))]
    async fn group_ids_synced_to_team(&self, team_id: &str) -> Result<Vec<String>, StoreError> {
        let ids = sqlx::query_scalar!(
            r#"
            SELECT ug.id AS "id!"
              FROM usergroups ug
              JOIN groupteams gs ON gs.groupid = ug.id
             WHERE gs.teamid = $1 AND gs.deleteat = 0 AND ug.deleteat = 0
            "#,
            team_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db(format!("failed to find Groups with teamId={team_id}")))?;
        tracing::Span::current().record("found", ids.len());
        Ok(ids)
    }
}

/// One row of `UPDATE … RETURNING` in `SqlTeamStore.UpdateMembersRole` (team_store.go:1691) —
/// `teamMemberSliceColumns`, the eight stored columns and **no scheme-role join**.
struct UpdatedTeamMemberRow {
    teamid: String,
    userid: String,
    roles: Option<String>,
    deleteat: Option<i64>,
    schemeuser: Option<bool>,
    schemeadmin: Option<bool>,
    schemeguest: Option<bool>,
    createat: Option<i64>,
}

impl crate::team_store::SqlTeamStore {
    /// Port of `SqlTeamStore.UpdateMembersRole` (team_store.go:1691): make exactly `admin_ids`
    /// the scheme admins of the team, touching only the rows whose flag changes, and return those
    /// rows.
    ///
    /// # The returned members are the raw row, not `ToModel`
    ///
    /// Go scans `RETURNING` straight into `model.TeamMember`: `Roles` is the stored column (the
    /// explicit roles, in practice) and `ExplicitRoles` stays empty, because no scheme is joined.
    /// The members go into `member_role_updated` websocket events, so that shape is on the wire
    /// and is reproduced rather than derived.
    ///
    /// Squirrel renders `sq.Eq{"UserId": []}` as `(1=0)` and `sq.NotEq` on the empty slice as
    /// `(1=1)`; `= ANY(ARRAY[]::text[])` and its negation agree, so an empty `admin_ids` demotes
    /// every admin, as Go does. Guests are never touched, NULL `SchemeGuest` counting as false.
    #[tracing::instrument(skip(self, admin_ids), fields(team_id = team_id, admins = admin_ids.len(), updated))]
    pub async fn update_members_role(
        &self,
        team_id: &str,
        admin_ids: &[String],
    ) -> Result<Vec<mm_model::team_member::TeamMember>, StoreError> {
        let rows = sqlx::query_as!(
            UpdatedTeamMemberRow,
            r#"
            UPDATE teammembers
               SET schemeadmin = CASE WHEN userid = ANY($2) THEN true ELSE false END
             WHERE teamid = $1
               AND deleteat = 0
               AND (schemeguest = false OR schemeguest IS NULL)
               AND ((schemeadmin = false AND userid = ANY($2))
                    OR (schemeadmin = true AND NOT (userid = ANY($2))))
            RETURNING teamid, userid, roles, deleteat, schemeuser, schemeadmin, schemeguest, createat
            "#,
            team_id,
            admin_ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db("failed to update TeamMembers".to_owned()))?;
        tracing::Span::current().record("updated", rows.len());
        Ok(rows
            .into_iter()
            .map(|r| mm_model::team_member::TeamMember {
                team_id: r.teamid,
                user_id: r.userid,
                roles: r.roles.unwrap_or_default(),
                delete_at: r.deleteat.unwrap_or_default(),
                scheme_guest: r.schemeguest.unwrap_or_default(),
                scheme_user: r.schemeuser.unwrap_or_default(),
                scheme_admin: r.schemeadmin.unwrap_or_default(),
                explicit_roles: String::new(),
                create_at: r.createat.unwrap_or_default(),
            })
            .collect())
    }
}

/// One row of `UPDATE … RETURNING` in `SqlChannelStore.UpdateMembersRole`
/// (channel_store.go:4470) — `channelMemberSliceColumns`, fifteen stored columns, no scheme join.
struct UpdatedChannelMemberRow {
    channelid: String,
    userid: String,
    roles: Option<String>,
    lastviewedat: Option<i64>,
    msgcount: Option<i64>,
    msgcountroot: Option<i64>,
    mentioncount: Option<i64>,
    mentioncountroot: Option<i64>,
    urgentmentioncount: Option<i64>,
    notifyprops: Option<serde_json::Value>,
    lastupdateat: Option<i64>,
    schemeuser: Option<bool>,
    schemeadmin: Option<bool>,
    schemeguest: Option<bool>,
    autotranslationdisabled: Option<bool>,
}

impl crate::channel_store::SqlChannelStore {
    /// Port of `SqlChannelStore.UpdateMembersRole` (channel_store.go:4470) — the channel twin
    /// of [`crate::team_store::SqlTeamStore::update_members_role`], with one difference in the
    /// predicate: **no `DeleteAt = 0`**, because `ChannelMembers` has no such column. Same raw
    /// row shape on the returned members, for the same reason.
    #[tracing::instrument(skip(self, admin_ids), fields(channel_id = channel_id, admins = admin_ids.len(), updated))]
    pub async fn update_members_role(
        &self,
        channel_id: &str,
        admin_ids: &[String],
    ) -> Result<Vec<mm_model::channel_member::ChannelMember>, StoreError> {
        let rows = sqlx::query_as!(
            UpdatedChannelMemberRow,
            r#"
            UPDATE channelmembers
               SET schemeadmin = CASE WHEN userid = ANY($2) THEN true ELSE false END
             WHERE channelid = $1
               AND (schemeguest = false OR schemeguest IS NULL)
               AND ((schemeadmin = false AND userid = ANY($2))
                    OR (schemeadmin = true AND NOT (userid = ANY($2))))
            RETURNING channelid, userid, roles, lastviewedat, msgcount, msgcountroot, mentioncount,
                      mentioncountroot, urgentmentioncount, notifyprops, lastupdateat, schemeuser,
                      schemeadmin, schemeguest, autotranslationdisabled
            "#,
            channel_id,
            admin_ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db("failed to update ChannelMembers".to_owned()))?;
        tracing::Span::current().record("updated", rows.len());
        rows.into_iter()
            .map(|r| {
                // `jsonb` NULL and JSON `null` both scan to a nil map in Go — see
                // `channel_member_from_row` for the [D-135] history.
                let notify_props = match r.notifyprops {
                    None | Some(serde_json::Value::Null) => None,
                    Some(value) => Some(
                        serde_json::from_value::<mm_model::utils::StringMap>(value).map_err(
                            |source| StoreError::Decode {
                                entity: "ChannelMember",
                                column: "notifyprops",
                                source,
                            },
                        )?,
                    ),
                };
                Ok(mm_model::channel_member::ChannelMember {
                    channel_id: r.channelid,
                    user_id: r.userid,
                    roles: r.roles.unwrap_or_default(),
                    last_viewed_at: r.lastviewedat.unwrap_or_default(),
                    msg_count: r.msgcount.unwrap_or_default(),
                    mention_count: r.mentioncount.unwrap_or_default(),
                    mention_count_root: r.mentioncountroot.unwrap_or_default(),
                    urgent_mention_count: r.urgentmentioncount.unwrap_or_default(),
                    msg_count_root: r.msgcountroot.unwrap_or_default(),
                    notify_props,
                    last_update_at: r.lastupdateat.unwrap_or_default(),
                    scheme_guest: r.schemeguest.unwrap_or_default(),
                    scheme_user: r.schemeuser.unwrap_or_default(),
                    scheme_admin: r.schemeadmin.unwrap_or_default(),
                    explicit_roles: String::new(),
                    auto_translation_disabled: r.autotranslationdisabled.unwrap_or_default(),
                })
            })
            .collect()
    }
}
