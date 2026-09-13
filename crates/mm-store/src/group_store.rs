//! Port of four reads from `SqlGroupStore` (channels/store/sqlstore/group_store.go).
//!
//! # Why four methods and not a group store
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
//!
//! # The three added for `members_minus_group_members`
//!
//! `GET /api/v4/channels/{channel_id}/members_minus_group_members` (api4/channel.go:2881) is the
//! **one route in `api4/channel.go` that reads the group tables and is not licence-gated** — a
//! licensed-feature question answered on an unlicensed server, which is why it is served here
//! rather than forwarded like everything in `api4/group.go`.
//!
//! ## Go interpolates the group ids into the SQL; this does not
//!
//! `channelMembersMinusGroupMembersQuery` builds its subquery predicate with
//! `fmt.Sprintf("GroupMembers.GroupId IN ('%s')", strings.Join(groupIDs, "', '"))`
//! (group_store.go:1752) — string interpolation, not a bound parameter. The handler validates
//! every id with `IsValidId` first, so nothing quotable reaches it, but the shape is reproduced
//! here as `= ANY($2)` over a bound `text[]`. That is the same set membership with the same
//! answer for every input the route admits, and it is **not** the same for an id containing a
//! quote — which the route rejects with a 400 before the store is reached.
//!
//! ## An empty `groupIDs` is unreachable, and would not mean the same thing
//!
//! Go's `Join` of an empty slice yields `IN ('')`, which matches no group; `= ANY(ARRAY[]::text[])`
//! is also empty. They agree. The handler's `len(groupIDsParam) < 26` check makes it moot.

use mm_model::group::{Group, GroupSource};
use mm_model::user::UserWithGroups;
use sqlx::PgPool;

use crate::error::StoreError;
use crate::user_store::{UserRow, user_from_row};

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

    /// Port of `SqlGroupStore.ChannelMembersMinusGroupMembers` (group_store.go:1777).
    ///
    /// The members of `channel_id` who are in **none** of `group_ids`, ordered by username, one
    /// page at a time. `page` is a page number and not an offset — Go's `Offset(page * perPage)`.
    fn channel_members_minus_group_members(
        &self,
        channel_id: &str,
        group_ids: &[String],
        page: i64,
        per_page: i64,
    ) -> impl std::future::Future<Output = Result<Vec<UserWithGroups>, StoreError>> + Send;

    /// Port of `SqlGroupStore.CountChannelMembersMinusGroupMembers` (group_store.go:1791).
    ///
    /// **`count(DISTINCT Users.Id)`, not `count(*)`.** The page query de-duplicates with a
    /// `GROUP BY` it does not have, so a member of three groups would otherwise be counted three
    /// times and the total would exceed the number of rows any page can return.
    fn count_channel_members_minus_group_members(
        &self,
        channel_id: &str,
        group_ids: &[String],
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetByIDs` (group_store.go:323).
    ///
    /// **No `DeleteAt` filter**, unlike most of this store's reads: a soft-deleted group still
    /// resolves here, because the caller is hydrating ids that a membership row already named.
    fn get_groups_by_ids(
        &self,
        group_ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Group>, StoreError>> + Send;
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

    #[tracing::instrument(skip(self, group_ids), fields(channel_id = %channel_id, groups = group_ids.len(), users))]
    async fn channel_members_minus_group_members(
        &self,
        channel_id: &str,
        group_ids: &[String],
        page: i64,
        per_page: i64,
    ) -> Result<Vec<UserWithGroups>, StoreError> {
        // `channelMembersMinusGroupMembersQuery(..., isCount: false)` (group_store.go:1731).
        //
        // Four predicates and two of them are easy to drop by accident:
        //
        //   - `Bots.UserId IS NULL` — the LEFT JOIN to `Bots` exists **only** to express this.
        //     Without it every integration in the channel would be reported as a member who
        //     would be removed by a group constraint.
        //   - `Users.DeleteAt = 0` and `Channels.DeleteAt = 0` — a deactivated member and an
        //     archived channel each contribute nothing.
        //
        // The two LEFT JOINs onto `GroupMembers`/`UserGroups` are what feed `string_agg`, and
        // they are LEFT because a member in no group at all must still appear in the answer —
        // they are precisely the users this route exists to list.
        //
        // The three bot columns `user_from_row` wants are not in Go's `getUsersColumns()` and are
        // not selected here either: `UserWithGroups` carries a zero-valued `IsBot` on this path,
        // which is also the truth, since `Bots.UserId IS NULL` excluded every bot.
        // `Offset(uint64(page * perPage))` (group_store.go:1779). `wrapping_mul`, not saturating:
        // Go's `int` multiply wraps too, and the 500 that a wrapped offset produces is the
        // answer both servers give — see `mm_api::channels::page_offset`.
        let offset = page.wrapping_mul(per_page);
        let rows = sqlx::query!(
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   COALESCE(cm.schemeguest, FALSE) AS "schemeguest!",
                   cm.schemeadmin,
                   cm.schemeuser,
                   string_agg(ug.id, ',') AS groupids
              FROM channelmembers cm
              JOIN channels c ON c.id = cm.channelid
              JOIN users u ON u.id = cm.userid
              LEFT JOIN bots b ON b.userid = cm.userid
              LEFT JOIN groupmembers gm ON gm.userid = u.id
              LEFT JOIN usergroups ug ON ug.id = gm.groupid
             WHERE c.deleteat = 0
               AND u.deleteat = 0
               AND b.userid IS NULL
               AND c.id = $1
               AND u.id NOT IN (
                     SELECT igm.userid
                       FROM groupmembers igm
                       JOIN usergroups iug ON iug.id = igm.groupid
                      WHERE igm.deleteat = 0
                        AND igm.groupid = ANY($2)
                   )
             GROUP BY u.id, cm.schemeguest, cm.schemeadmin, cm.schemeuser
             ORDER BY u.username ASC
             LIMIT $3 OFFSET $4
            "#,
            channel_id,
            group_ids,
            per_page,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find UserWithGroups".to_owned(),
            source,
        })?;

        tracing::Span::current().record("users", rows.len());
        rows.into_iter()
            .map(|row| {
                let user = user_from_row(UserRow {
                    id: row.id,
                    createat: row.createat,
                    updateat: row.updateat,
                    deleteat: row.deleteat,
                    username: row.username,
                    password: row.password,
                    authdata: row.authdata,
                    authservice: row.authservice,
                    email: row.email,
                    emailverified: row.emailverified,
                    nickname: row.nickname,
                    firstname: row.firstname,
                    lastname: row.lastname,
                    position: row.position,
                    roles: row.roles,
                    allowmarketing: row.allowmarketing,
                    props: row.props,
                    notifyprops: row.notifyprops,
                    lastpasswordupdate: row.lastpasswordupdate,
                    lastpictureupdate: row.lastpictureupdate,
                    failedattempts: row.failedattempts,
                    locale: row.locale,
                    timezone: row.timezone,
                    mfaactive: row.mfaactive,
                    mfasecret: row.mfasecret,
                    mfausedtimestamps: row.mfausedtimestamps,
                    remoteid: row.remoteid,
                    lastlogin: row.lastlogin,
                    isbot: false,
                    botdescription: String::new(),
                    botlasticonupdate: 0,
                })?;
                Ok(UserWithGroups {
                    user,
                    group_ids: row.groupids,
                    // Go scans into a zero-valued struct, so the slice is nil here and the app
                    // layer replaces it with `[]` before it reaches the wire.
                    groups: None,
                    scheme_guest: row.schemeguest,
                    // `SchemeAdmin` and `SchemeUser` are selected raw where `SchemeGuest` is
                    // `COALESCE`d; the columns are `NOT NULL` so the asymmetry is unobservable,
                    // and it is reproduced rather than harmonised.
                    scheme_admin: row.schemeadmin.unwrap_or_default(),
                    scheme_user: row.schemeuser.unwrap_or_default(),
                })
            })
            .collect()
    }

    #[tracing::instrument(skip(self, group_ids), fields(channel_id = %channel_id, groups = group_ids.len(), count))]
    async fn count_channel_members_minus_group_members(
        &self,
        channel_id: &str,
        group_ids: &[String],
    ) -> Result<i64, StoreError> {
        // The same query with `isCount: true` — `count(DISTINCT Users.Id)` in place of the
        // columns, and **no `GROUP BY`**. The `DISTINCT` is what the page query's `GROUP BY`
        // does: a user in three groups produces three joined rows either way.
        let count = sqlx::query_scalar!(
            r#"
            SELECT count(DISTINCT u.id) AS "count!"
              FROM channelmembers cm
              JOIN channels c ON c.id = cm.channelid
              JOIN users u ON u.id = cm.userid
              LEFT JOIN bots b ON b.userid = cm.userid
              LEFT JOIN groupmembers gm ON gm.userid = u.id
              LEFT JOIN usergroups ug ON ug.id = gm.groupid
             WHERE c.deleteat = 0
               AND u.deleteat = 0
               AND b.userid IS NULL
               AND c.id = $1
               AND u.id NOT IN (
                     SELECT igm.userid
                       FROM groupmembers igm
                       JOIN usergroups iug ON iug.id = igm.groupid
                      WHERE igm.deleteat = 0
                        AND igm.groupid = ANY($2)
                   )
            "#,
            channel_id,
            group_ids
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count ChannelMembers".to_owned(),
            source,
        })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }

    #[tracing::instrument(skip(self, group_ids), fields(asked = group_ids.len(), found))]
    async fn get_groups_by_ids(&self, group_ids: &[String]) -> Result<Vec<Group>, StoreError> {
        // `userGroupsSelectQuery.Where(sq.Eq{"Id": groupIDs})` (group_store.go:325) — the ten
        // stored columns, and **no `DeleteAt` filter**. The five `db:"-"` fields on the model
        // (`has_syncables`, the three counts and `member_ids`) are computed by other queries and
        // stay zero here, which is what Go's scan leaves them as too.
        let rows = sqlx::query!(
            r#"
            SELECT ug.id,
                   ug.name,
                   ug.displayname,
                   ug.description,
                   ug.source,
                   ug.remoteid,
                   ug.createat,
                   ug.updateat,
                   ug.deleteat,
                   ug.allowreference
              FROM usergroups ug
             WHERE ug.id = ANY($1)
            "#,
            group_ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Groups by ids".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows
            .into_iter()
            .map(|row| Group {
                id: row.id,
                name: row.name,
                display_name: row.displayname.unwrap_or_default(),
                description: row.description.unwrap_or_default(),
                source: GroupSource(row.source.unwrap_or_default()),
                remote_id: row.remoteid,
                create_at: row.createat.unwrap_or_default(),
                update_at: row.updateat.unwrap_or_default(),
                delete_at: row.deleteat.unwrap_or_default(),
                allow_reference: row.allowreference.unwrap_or_default(),
                has_syncables: false,
                member_count: None,
                channel_member_count: None,
                channel_member_timezones_count: None,
                member_ids: None,
            })
            .collect())
    }
}
