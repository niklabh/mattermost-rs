//! Port of one read from `SqlGroupStore` (channels/store/sqlstore/group_store.go).
//!
//! # Why one method and not a group store
//!
//! `addUserToChannel` (app/channel.go:1881) decides a brand-new member's `SchemeAdmin` flag by
//! asking `UserIsInAdminRoleGroup` — is this user in a group that is synced to this channel with
//! `SchemeAdmin` set? On a server with no LDAP/SAML group sync the answer is always `false` and
//! the tables are empty, but the *question* is on the add path unconditionally, so a port that
//! assumed `false` would be guessing on exactly the deployment where it matters.
//!
//! That is the whole of the group surface `POST /channels/{id}/members` needs. The rest of
//! `GroupStore` — 40-odd methods driving group syncables, the group API and the sync job — waits
//! for the routes that read it. See CLAUDE.md on porting a route's needs, not a file.

use sqlx::PgPool;

use crate::error::StoreError;

/// Port of the `AdminRoleGroupsForSyncableMember` slice of `store.GroupStore`.
pub trait GroupStore {
    /// Port of `SqlGroupStore.AdminRoleGroupsForSyncableMember` (group_store.go:1805) for
    /// `model.GroupSyncableTypeChannel`.
    ///
    /// Go switches on the syncable type to pick `GroupChannels`/`ChannelId` or
    /// `GroupTeams`/`TeamId`, and returns `errors.New("invalid syncable type")` for anything else.
    /// Only the channel arm has a caller here, so only the channel arm exists — the type
    /// parameter would be a constant, and an unreachable error branch.
    fn admin_role_groups_for_channel_member(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;

    /// Port of `SqlGroupStore.AdminRoleGroupsForSyncableMember` (group_store.go:1805) for
    /// `model.GroupSyncableTypeTeam` — the `GroupTeams`/`TeamId` arm of the same switch.
    ///
    /// `TeamService.JoinUserToTeam` (app/teams/teams.go:186) asks this to decide a brand-new
    /// member's `SchemeAdmin` flag, and it asks it **only for a non-guest**. A guest keeps
    /// `SchemeAdmin` false whatever their groups say.
    fn admin_role_groups_for_team_member(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlGroupStore {
    pool: PgPool,
}

impl SqlGroupStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl GroupStore for SqlGroupStore {
    #[tracing::instrument(skip(self), fields(user_id = %user_id, channel_id = %channel_id, groups))]
    async fn admin_role_groups_for_channel_member(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        // All four of Go's equality predicates, and each one is load-bearing:
        //
        //   - `GroupMembers.DeleteAt = 0` — a user removed from the group is not an admin.
        //   - `GroupChannels.DeleteAt = 0` — an unlinked syncable grants nothing.
        //   - `GroupChannels.SchemeAdmin = TRUE` — the *point* of the query; without it every
        //     synced group member would be made a channel admin on join.
        //
        // The join is INNER on both sides, so a group with no syncable row contributes nothing.
        let ids = sqlx::query_scalar!(
            r#"
            SELECT gm.groupid AS "groupid!"
              FROM groupmembers gm
              INNER JOIN groupchannels jg ON jg.groupid = gm.groupid
             WHERE gm.userid = $1
               AND gm.deleteat = 0
               AND jg.channelid = $2
               AND jg.deleteat = 0
               AND jg.schemeadmin = TRUE
            "#,
            user_id,
            channel_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to find Group ids for userId={user_id} channelId={channel_id}"
            ),
            source,
        })?;

        tracing::Span::current().record("groups", ids.len());
        Ok(ids)
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id, groups))]
    async fn admin_role_groups_for_team_member(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        // The channel arm's four predicates with `GroupTeams`/`TeamId` substituted, and each one
        // carries the same weight: without `jg.schemeadmin = TRUE` every synced group member
        // would join their team as a team admin.
        let ids = sqlx::query_scalar!(
            r#"
            SELECT gm.groupid AS "groupid!"
              FROM groupmembers gm
              INNER JOIN groupteams jg ON jg.groupid = gm.groupid
             WHERE gm.userid = $1
               AND gm.deleteat = 0
               AND jg.teamid = $2
               AND jg.deleteat = 0
               AND jg.schemeadmin = TRUE
            "#,
            user_id,
            team_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Group ids for userId={user_id} teamId={team_id}"),
            source,
        })?;

        tracing::Span::current().record("groups", ids.len());
        Ok(ids)
    }
}
