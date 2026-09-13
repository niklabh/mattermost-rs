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
//! # The five added for `members_minus_group_members`
//!
//! `GET /api/v4/channels/{channel_id}/members_minus_group_members` (api4/channel.go:2881) and
//! `GET /api/v4/teams/{team_id}/members_minus_group_members` (api4/team.go:2222) are the **two
//! routes outside `api4/group.go` that read the group tables and are not licence-gated** — a
//! licensed-feature question answered on an unlicensed server, which is why they are served here
//! rather than forwarded like everything in `api4/group.go`.
//!
//! The two store queries are `teamMembersMinusGroupMembersQuery` and its channel twin, which
//! differ by exactly one predicate (`TeamMembers.DeleteAt = 0`, which has no channel counterpart)
//! and by which membership table the three scheme flags come from. Their column lists are
//! identical, hence one [`MinusGroupRow`].
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

use mm_model::group::{
    Group, GroupSearchOpts, GroupSource, GroupWithSchemeAdmin, GroupWithUserIds,
    get_syncable_group_source_prefixes, get_syncable_group_sources,
};
use mm_model::group_member::GroupMember;
use mm_model::user::{UserWithGroups, ViewUsersRestrictions};
use mm_model::utils::{get_millis, new_id};
use sqlx::PgPool;

use crate::error::StoreError;
use crate::user_store::{UserRow, sanitize_search_term, user_from_row};

/// The row-to-model half of `teamMembersMinusGroupMembersQuery` and its channel twin.
///
/// The two queries select the **same** column list — Go's `getUsersColumns()`, then the three
/// scheme flags off whichever membership table, then `string_agg(UserGroups.Id, ',')` — so they
/// share one mapping. A macro rather than a function because `sqlx::query!` produces an anonymous
/// row type per call site that no signature can name.
///
/// **`sqlx::query_as!` with a named struct would be the obvious alternative and is the wrong one
/// here**: it binds columns to fields by *position*, which turns "reorder two independent SELECT
/// columns" — the no-op control both mutation plans rely on — into a silent value swap. A control
/// that can fail makes every verdict in the run meaningless, so the anonymous, name-addressed row
/// stays.
///
/// The three bot columns `user_from_row` wants are not in `getUsersColumns()` and are not selected
/// by either query: `UserWithGroups` carries a zero-valued `IsBot` on this path, which is also the
/// truth, since `Bots.UserId IS NULL` excluded every bot.
macro_rules! minus_group_row {
    ($row:expr) => {{
        let row = $row;
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
            // Go scans into a zero-valued struct, so the slice is nil here and the app layer
            // replaces it with `[]` before it reaches the wire.
            groups: None,
            scheme_guest: row.schemeguest,
            // `SchemeAdmin` and `SchemeUser` are selected raw where `SchemeGuest` is `COALESCE`d;
            // the columns are `NOT NULL` so the asymmetry is unobservable, and it is reproduced
            // rather than harmonised.
            scheme_admin: row.schemeadmin.unwrap_or_default(),
            scheme_user: row.schemeuser.unwrap_or_default(),
        })
    }};
}

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

    /// Port of `SqlGroupStore.TeamMembersMinusGroupMembers` (group_store.go:1703).
    ///
    /// The channel method's team twin, and **not** the same predicate set: the team query adds
    /// `TeamMembers.DeleteAt = 0`, which has no channel counterpart because `ChannelMembers` has
    /// no `DeleteAt` column at all. A member removed from the team leaves the row behind with a
    /// stamp on it, so without that predicate a former member is reported as someone a group
    /// constraint would remove.
    fn team_members_minus_group_members(
        &self,
        team_id: &str,
        group_ids: &[String],
        page: i64,
        per_page: i64,
    ) -> impl std::future::Future<Output = Result<Vec<UserWithGroups>, StoreError>> + Send;

    /// Port of `SqlGroupStore.CountTeamMembersMinusGroupMembers` (group_store.go:1717).
    ///
    /// `count(DISTINCT Users.Id)`, for the reason the channel twin documents: the page query
    /// de-duplicates with a `GROUP BY` the count query does not have.
    fn count_team_members_minus_group_members(
        &self,
        team_id: &str,
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

    // --- The write surface behind the seven CRUD and membership routes of `api4/group.go`,
    // ported 2026-09-13 with the licensed pair as the oracle ([D-360]). The syncable methods
    // (`GetGroupSyncable` and its three siblings) are a separate port ([D-390]).

    /// Port of `SqlGroupStore.Get` (group_store.go:275) — by id, **any** `DeleteAt`.
    ///
    /// A miss is `ErrNotFound("Group", id)`, which every app caller turns into
    /// `app.group.no_rows` at 404. A soft-deleted group is found: `deleteGroup` on it then fails
    /// in [`GroupStore::delete`], not here.
    fn get(
        &self,
        group_id: &str,
    ) -> impl std::future::Future<Output = Result<Group, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetByName` (group_store.go:290). `filter_allow_reference` is the
    /// one field of `GroupSearchOpts` it reads. A miss names `name=<name>`, not the bare name.
    fn get_by_name(
        &self,
        name: &str,
        filter_allow_reference: bool,
    ) -> impl std::future::Future<Output = Result<Group, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetByNames` (group_store.go:309). No `DeleteAt` filter — a
    /// soft-deleted group is still returned by name — and no ordering.
    fn get_by_names(
        &self,
        names: &[String],
        filter_allow_reference: bool,
    ) -> impl std::future::Future<Output = Result<Vec<Group>, StoreError>> + Send;

    /// Port of `SqlGroupStore.CreateWithUserIds` (group_store.go:141): validate, check every
    /// user exists and is active, then insert the group and its members in one transaction and
    /// read the group back with its member count.
    ///
    /// Three refusals before the insert, in Go's order: a non-empty `Id` is `ErrInvalidInput`,
    /// `IsValidForCreate` is passed through as its own `AppError`, and a user that does not exist
    /// (or is deactivated) is `ErrNotFound("User", id)`. A taken name is `ErrUniqueConstraint`.
    fn create_with_user_ids(
        &self,
        group: GroupWithUserIds,
    ) -> impl std::future::Future<Output = Result<Group, StoreError>> + Send;

    /// Port of `SqlGroupStore.Update` (group_store.go:386): re-read the row, refuse a `DeleteAt`
    /// that changed to anything but 0, keep the stored `CreateAt`, stamp `UpdateAt`, validate,
    /// write **every** column. Returns the input with those two timestamps set — not a re-read —
    /// so the `db:"-"` fields are whatever the caller passed in.
    fn update(
        &self,
        group: Group,
    ) -> impl std::future::Future<Output = Result<Group, StoreError>> + Send;

    /// Port of `SqlGroupStore.Delete` (group_store.go:428): a soft delete of a group that is
    /// **not already deleted**, stamping `DeleteAt` and `UpdateAt` with one clock reading. An
    /// already-deleted group is `ErrNotFound`.
    fn delete(
        &self,
        group_id: &str,
    ) -> impl std::future::Future<Output = Result<Group, StoreError>> + Send;

    /// Port of `SqlGroupStore.Restore` (group_store.go:455): the mirror of [`GroupStore::delete`]
    /// — a group that is **not** deleted is `ErrNotFound`.
    fn restore(
        &self,
        group_id: &str,
    ) -> impl std::future::Future<Output = Result<Group, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetMember` (group_store.go:481) — the **active** membership row.
    ///
    /// Go wraps `sql.ErrNoRows` and its one caller (`SessionHasPermissionToGroup`) tests for it
    /// with `errors.Is`, treating "not a member" as a fact and every other failure as a refusal.
    /// `Option` says the first; `Err` the second.
    fn get_member(
        &self,
        group_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<GroupMember>, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetMemberCount` (group_store.go:590) — `COUNT(DISTINCT Users.Id)`
    /// over active memberships of **active** users.
    fn get_member_count(
        &self,
        group_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlGroupStore.UpsertMembers` (group_store.go:1921): every user must exist and be
    /// active (`ErrNotFound("User", id)`), then one insert with `ON CONFLICT (groupid, userid)
    /// DO UPDATE` that resurrects a soft-deleted membership. The rows come back in **request
    /// order** with one `CreateAt` for all of them.
    ///
    /// A user id listed twice makes Postgres refuse the whole statement ("cannot affect row a
    /// second time"), which Go surfaces as a plain failure and the app layer as a 500 — kept,
    /// because it is what a client sees.
    fn upsert_members(
        &self,
        group_id: &str,
        user_ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<GroupMember>, StoreError>> + Send;

    /// Port of `SqlGroupStore.DeleteMembers` (group_store.go:1984): read the **active**
    /// memberships named, refuse with `ErrNotFound("User", id)` for any id that has none, then
    /// stamp `DeleteAt` on them. The rows come back in the order the `SELECT` produced them,
    /// which has no `ORDER BY` in Go and none here.
    fn delete_members(
        &self,
        group_id: &str,
        user_ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<GroupMember>, StoreError>> + Send;

    // --- The three list reads behind `getGroupsAllowedForReferenceInChannel`
    // (app/notification.go:1498), ported 2026-09-13 with the licensed Go oracle as the parity
    // oracle (`parity/group_reads_licensed`). They are also what `GET /api/v4/groups`,
    // `GET /channels/{id}/groups` and `GET /teams/{id}/groups` read, so every option branch of
    // Go's builders is ported, not only the two combinations that caller passes.

    /// Port of `SqlGroupStore.GetGroups` (group_store.go:1472).
    ///
    /// **`per_page == 0` means no `LIMIT` at all** — Go adds `Limit`/`Offset` only when
    /// `perPage != 0`, and `getGroupsAllowedForReferenceInChannel` passes `0, 0` to read every
    /// group. `page` is a page number, not an offset (`Offset(page * perPage)`).
    ///
    /// Every option of `GroupSearchOpts` that Go's builder reads is honoured; the ones it does
    /// **not** read are `page_opts` (this method takes the page as arguments) and
    /// `include_member_ids`, which `App.GetGroups` (app/group.go:660) resolves with a query per
    /// group after the fact. The optional `view_restrictions` narrows **only the member count**
    /// (`applyViewRestrictionsFilter` on the count subquery, user_store.go:2162), never which
    /// groups are returned.
    ///
    /// `member_count`, `channel_member_count` and `channel_member_timezones_count` are `Some`
    /// exactly when their option was set, since each is `omitempty` on the wire.
    fn get_groups(
        &self,
        page: i64,
        per_page: i64,
        opts: &GroupSearchOpts,
        view_restrictions: Option<&ViewUsersRestrictions>,
    ) -> impl std::future::Future<Output = Result<Vec<Group>, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetGroupsByChannel` (group_store.go:1234): the **live** groups
    /// linked to `channel_id` by a **live** `GroupChannels` row, each carrying that row's
    /// `SchemeAdmin` (`false` when the column is null — Go's `ToModel` substitutes it).
    ///
    /// Of `GroupSearchOpts`, `groupsBySyncableBaseQuery` reads four: `include_member_count`,
    /// `filter_allow_reference`, `q` and `page_opts`. **There is no `ORDER BY` unless one of
    /// `include_member_count` or `page_opts` is set** — a caller with neither gets Postgres's
    /// order, in Go as here.
    fn get_groups_by_channel(
        &self,
        channel_id: &str,
        opts: &GroupSearchOpts,
    ) -> impl std::future::Future<Output = Result<Vec<GroupWithSchemeAdmin>, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetGroupsByTeam` (group_store.go:1433) — [`GroupStore::get_groups_by_channel`]
    /// over `GroupTeams`/`TeamId`; the same four options, the same conditional ordering.
    fn get_groups_by_team(
        &self,
        team_id: &str,
        opts: &GroupSearchOpts,
    ) -> impl std::future::Future<Output = Result<Vec<GroupWithSchemeAdmin>, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlGroupStore {
    /// Shared with `group_syncable_store.rs`, which implements the syncable half of Go's
    /// `SqlGroupStore` on the same type.
    pub(crate) pool: PgPool,
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
        rows.into_iter().map(|row| minus_group_row!(row)).collect()
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

    #[tracing::instrument(skip(self, group_ids), fields(team_id = %team_id, groups = group_ids.len(), users))]
    async fn team_members_minus_group_members(
        &self,
        team_id: &str,
        group_ids: &[String],
        page: i64,
        per_page: i64,
    ) -> Result<Vec<UserWithGroups>, StoreError> {
        // `teamMembersMinusGroupMembersQuery(..., isCount: false)` (group_store.go:1657) — the
        // channel query with `TeamMembers`/`Teams` substituted and **one predicate more**:
        //
        //   - `TeamMembers.DeleteAt = 0`. `ChannelMembers` has no such column, so the channel
        //     twin has nothing to copy here. Nothing in api4 writes a non-zero value either —
        //     `SqlTeamStore.RemoveMember` issues a `DELETE` (team_store.go:1273) — so the parity
        //     suite plants one by SQL, which is the only way this predicate can be made to bite.
        //   - `Teams.DeleteAt = 0` is the twin of `Channels.DeleteAt = 0`: a deleted team answers
        //     an empty page, not its membership.
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
                   COALESCE(tm.schemeguest, FALSE) AS "schemeguest!",
                   tm.schemeadmin,
                   tm.schemeuser,
                   string_agg(ug.id, ',') AS groupids
              FROM teammembers tm
              JOIN teams t ON t.id = tm.teamid
              JOIN users u ON u.id = tm.userid
              LEFT JOIN bots b ON b.userid = tm.userid
              LEFT JOIN groupmembers gm ON gm.userid = u.id
              LEFT JOIN usergroups ug ON ug.id = gm.groupid
             WHERE tm.deleteat = 0
               AND t.deleteat = 0
               AND u.deleteat = 0
               AND b.userid IS NULL
               AND t.id = $1
               AND u.id NOT IN (
                     SELECT igm.userid
                       FROM groupmembers igm
                       JOIN usergroups iug ON iug.id = igm.groupid
                      WHERE igm.deleteat = 0
                        AND igm.groupid = ANY($2)
                   )
             GROUP BY u.id, tm.schemeguest, tm.schemeadmin, tm.schemeuser
             ORDER BY u.username ASC
             LIMIT $3 OFFSET $4
            "#,
            team_id,
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
        rows.into_iter().map(|row| minus_group_row!(row)).collect()
    }

    #[tracing::instrument(skip(self, group_ids), fields(team_id = %team_id, groups = group_ids.len(), count))]
    async fn count_team_members_minus_group_members(
        &self,
        team_id: &str,
        group_ids: &[String],
    ) -> Result<i64, StoreError> {
        // `isCount: true`: `count(DISTINCT Users.Id)` and no `GROUP BY`, over the identical
        // five-predicate `WHERE`.
        let count = sqlx::query_scalar!(
            r#"
            SELECT count(DISTINCT u.id) AS "count!"
              FROM teammembers tm
              JOIN teams t ON t.id = tm.teamid
              JOIN users u ON u.id = tm.userid
              LEFT JOIN bots b ON b.userid = tm.userid
              LEFT JOIN groupmembers gm ON gm.userid = u.id
              LEFT JOIN usergroups ug ON ug.id = gm.groupid
             WHERE tm.deleteat = 0
               AND t.deleteat = 0
               AND u.deleteat = 0
               AND b.userid IS NULL
               AND t.id = $1
               AND u.id NOT IN (
                     SELECT igm.userid
                       FROM groupmembers igm
                       JOIN usergroups iug ON iug.id = igm.groupid
                      WHERE igm.deleteat = 0
                        AND igm.groupid = ANY($2)
                   )
            "#,
            team_id,
            group_ids
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count TeamMembers minus GroupMembers".to_owned(),
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

    #[tracing::instrument(skip(self), fields(group_id = %group_id))]
    async fn get(&self, group_id: &str) -> Result<Group, StoreError> {
        let row = sqlx::query_as!(
            GroupRow,
            r#"
            SELECT id, name, displayname, description, source, remoteid, createat, updateat,
                   deleteat, allowreference
              FROM usergroups
             WHERE id = $1
            "#,
            group_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Group with id={group_id}"),
            source,
        })?;
        row.map(GroupRow::into_group)
            .ok_or_else(|| StoreError::NotFound {
                entity: "Group",
                criteria: group_id.to_owned(),
            })
    }

    #[tracing::instrument(skip(self), fields(name = %name, filter_allow_reference))]
    async fn get_by_name(
        &self,
        name: &str,
        filter_allow_reference: bool,
    ) -> Result<Group, StoreError> {
        // `AllowReference = true` is appended only when the option is set; expressed as a bound
        // parameter so the statement stays compile-checked.
        let row = sqlx::query_as!(
            GroupRow,
            r#"
            SELECT id, name, displayname, description, source, remoteid, createat, updateat,
                   deleteat, allowreference
              FROM usergroups
             WHERE name = $1
               AND ($2 = FALSE OR allowreference = TRUE)
            "#,
            name,
            filter_allow_reference
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Group with name={name}"),
            source,
        })?;
        row.map(GroupRow::into_group)
            .ok_or_else(|| StoreError::NotFound {
                entity: "Group",
                criteria: format!("name={name}"),
            })
    }

    #[tracing::instrument(skip(self, names), fields(names = names.len(), filter_allow_reference, found))]
    async fn get_by_names(
        &self,
        names: &[String],
        filter_allow_reference: bool,
    ) -> Result<Vec<Group>, StoreError> {
        let rows = sqlx::query_as!(
            GroupRow,
            r#"
            SELECT id, name, displayname, description, source, remoteid, createat, updateat,
                   deleteat, allowreference
              FROM usergroups
             WHERE name = ANY($1)
               AND ($2 = FALSE OR allowreference = TRUE)
            "#,
            names,
            filter_allow_reference
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Groups by names".to_owned(),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(GroupRow::into_group).collect())
    }

    #[tracing::instrument(skip_all, fields(id))]
    async fn create_with_user_ids(&self, group: GroupWithUserIds) -> Result<Group, StoreError> {
        let GroupWithUserIds {
            mut group,
            user_ids,
        } = group;
        if !group.id.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "Group",
                field: "id",
                value: group.id,
            });
        }
        group
            .is_valid_for_create()
            .map_err(|app_error| StoreError::Invalid {
                entity: "Group",
                app_error,
            })?;
        let user_ids = user_ids.unwrap_or_default();
        self.check_users_exist(&user_ids).await?;

        group.id = new_id();
        group.create_at = get_millis();
        group.update_at = group.create_at;
        tracing::Span::current().record("id", &group.id);

        let mut txn = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        sqlx::query!(
            r#"
            INSERT INTO usergroups
                (id, name, displayname, description, source, remoteid, createat, updateat,
                 deleteat, allowreference)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0, $9)
            "#,
            group.id,
            group.name,
            group.display_name,
            group.description,
            group.source.as_str(),
            group.remote_id,
            group.create_at,
            group.update_at,
            group.allow_reference
        )
        .execute(&mut *txn)
        .await
        .map_err(|source| {
            if is_group_name_unique_violation(&source) {
                StoreError::Conflict {
                    resource: "Name",
                    source,
                }
            } else {
                StoreError::Db {
                    context: "failed to save Group".to_owned(),
                    source,
                }
            }
        })?;

        // `insertGroupUsers` (group_store.go:255): one `CreateAt` for the whole batch, chunked
        // in Go for the parameter limit — an array bind has no such limit.
        if !user_ids.is_empty() {
            let create_at = get_millis();
            sqlx::query!(
                r#"
                INSERT INTO groupmembers (groupid, userid, createat, deleteat)
                SELECT $1, unnest($2::text[]), $3, 0
                "#,
                group.id,
                &user_ids,
                create_at
            )
            .execute(&mut *txn)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to insert GroupMembers".to_owned(),
                source,
            })?;
        }

        // The read-back Go does inside the transaction: the ten columns plus a member count
        // that counts **every** `GroupMembers` row, deleted or not — a fresh group has none of
        // those, and the app layer overwrites the count anyway.
        let row = sqlx::query!(
            r#"
            SELECT ug.id, ug.name, ug.displayname, ug.description, ug.source, ug.remoteid,
                   ug.createat, ug.updateat, ug.deleteat, ug.allowreference,
                   (SELECT COUNT(gm.userid) FROM groupmembers gm WHERE gm.groupid = ug.id)
                       AS "membercount!"
              FROM usergroups ug
             WHERE ug.id = $1
            "#,
            group.id
        )
        .fetch_one(&mut *txn)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to read back Group with id={}", group.id),
            source,
        })?;
        txn.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })?;

        let mut created = GroupRow {
            id: row.id,
            name: row.name,
            displayname: row.displayname,
            description: row.description,
            source: row.source,
            remoteid: row.remoteid,
            createat: row.createat,
            updateat: row.updateat,
            deleteat: row.deleteat,
            allowreference: row.allowreference,
        }
        .into_group();
        created.member_count = Some(row.membercount);
        Ok(created)
    }

    #[tracing::instrument(skip_all, fields(id = %group.id))]
    async fn update(&self, mut group: Group) -> Result<Group, StoreError> {
        let stored = self.get(&group.id).await?;

        // "If updating DeleteAt it can only be to 0" — a bare `errors.New` in Go, which the app
        // layer reads as a 500. Nothing reachable through api4 can set it, since `Patch` never
        // touches `DeleteAt`.
        if group.delete_at != stored.delete_at && group.delete_at != 0 {
            return Err(StoreError::Argument {
                entity: "Group",
                detail: "DeleteAt should be 0 when updating",
            });
        }

        group.create_at = stored.create_at;
        group.update_at = get_millis();

        group
            .is_valid_for_update()
            .map_err(|app_error| StoreError::Invalid {
                entity: "Group",
                app_error,
            })?;

        let result = sqlx::query!(
            r#"
            UPDATE usergroups
               SET name = $2, displayname = $3, description = $4, source = $5, remoteid = $6,
                   createat = $7, updateat = $8, deleteat = $9, allowreference = $10
             WHERE id = $1
            "#,
            group.id,
            group.name,
            group.display_name,
            group.description,
            group.source.as_str(),
            group.remote_id,
            group.create_at,
            group.update_at,
            group.delete_at,
            group.allow_reference
        )
        .execute(&self.pool)
        .await
        .map_err(|source| {
            if is_group_name_unique_violation(&source) {
                StoreError::Conflict {
                    resource: "Name",
                    source,
                }
            } else {
                StoreError::Db {
                    context: "failed to update Group".to_owned(),
                    source,
                }
            }
        })?;
        if result.rows_affected() > 1 {
            return Err(StoreError::Argument {
                entity: "Group",
                detail: "multiple Groups were update",
            });
        }
        Ok(group)
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id))]
    async fn delete(&self, group_id: &str) -> Result<Group, StoreError> {
        let row = sqlx::query_as!(
            GroupRow,
            r#"
            SELECT id, name, displayname, description, source, remoteid, createat, updateat,
                   deleteat, allowreference
              FROM usergroups
             WHERE id = $1 AND deleteat = 0
            "#,
            group_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Group with id={group_id}"),
            source,
        })?;
        let mut group = row
            .map(GroupRow::into_group)
            .ok_or_else(|| StoreError::NotFound {
                entity: "Group",
                criteria: group_id.to_owned(),
            })?;

        let time = get_millis();
        group.delete_at = time;
        group.update_at = time;
        sqlx::query!(
            "UPDATE usergroups SET deleteat = $1, updateat = $2 WHERE id = $3 AND deleteat = 0",
            group.delete_at,
            group.update_at,
            group_id
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update Group with id={group_id}"),
            source,
        })?;
        Ok(group)
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id))]
    async fn restore(&self, group_id: &str) -> Result<Group, StoreError> {
        let row = sqlx::query_as!(
            GroupRow,
            r#"
            SELECT id, name, displayname, description, source, remoteid, createat, updateat,
                   deleteat, allowreference
              FROM usergroups
             WHERE id = $1 AND deleteat <> 0
            "#,
            group_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Group with id={group_id}"),
            source,
        })?;
        let mut group = row
            .map(GroupRow::into_group)
            .ok_or_else(|| StoreError::NotFound {
                entity: "Group",
                criteria: group_id.to_owned(),
            })?;

        group.update_at = get_millis();
        group.delete_at = 0;
        sqlx::query!(
            "UPDATE usergroups SET deleteat = 0, updateat = $1 WHERE id = $2 AND deleteat <> 0",
            group.update_at,
            group_id
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update Group with id={group_id}"),
            source,
        })?;
        Ok(group)
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id, user_id = %user_id, member))]
    async fn get_member(
        &self,
        group_id: &str,
        user_id: &str,
    ) -> Result<Option<GroupMember>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT groupid AS "groupid!", userid AS "userid!",
                   COALESCE(createat, 0) AS "createat!", COALESCE(deleteat, 0) AS "deleteat!"
              FROM groupmembers
             WHERE userid = $1 AND groupid = $2 AND deleteat = 0
            "#,
            user_id,
            group_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "GetMember".to_owned(),
            source,
        })?;
        tracing::Span::current().record("member", row.is_some());
        Ok(row.map(|row| GroupMember {
            group_id: row.groupid,
            user_id: row.userid,
            create_at: row.createat,
            delete_at: row.deleteat,
        }))
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id, count))]
    async fn get_member_count(&self, group_id: &str) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(DISTINCT u.id) AS "count!"
              FROM groupmembers gm
              JOIN users u ON u.id = gm.userid
             WHERE gm.groupid = $1 AND u.deleteat = 0 AND gm.deleteat = 0
            "#,
            group_id
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to count member Users for Group with id={group_id}"),
            source,
        })?;
        tracing::Span::current().record("count", count);
        Ok(count)
    }

    #[tracing::instrument(skip(self, user_ids), fields(group_id = %group_id, users = user_ids.len()))]
    async fn upsert_members(
        &self,
        group_id: &str,
        user_ids: &[String],
    ) -> Result<Vec<GroupMember>, StoreError> {
        // Go reads the group first and wraps a miss as a plain error, not `ErrNotFound` — the
        // app layer answers 500 to it. Unreachable through api4, which fetched the group already.
        let exists = sqlx::query_scalar!(
            "SELECT 1 AS \"one!\" FROM usergroups WHERE id = $1",
            group_id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get UserGroup with groupId={group_id}"),
            source,
        })?;
        if exists.is_none() {
            return Err(StoreError::Argument {
                entity: "UserGroup",
                detail: "no such group",
            });
        }

        self.check_users_exist(user_ids).await?;

        let create_at = get_millis();
        let members: Vec<GroupMember> = user_ids
            .iter()
            .map(|user_id| GroupMember {
                group_id: group_id.to_owned(),
                user_id: user_id.clone(), // the row carries its own copy of the id
                create_at,
                delete_at: 0,
            })
            .collect();

        if !members.is_empty() {
            sqlx::query!(
                r#"
                INSERT INTO groupmembers (groupid, userid, createat, deleteat)
                SELECT $1, unnest($2::text[]), $3, 0
                ON CONFLICT (groupid, userid) DO UPDATE SET createat = $3, deleteat = 0
                "#,
                group_id,
                user_ids,
                create_at
            )
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to save GroupMember".to_owned(),
                source,
            })?;
        }
        Ok(members)
    }

    #[tracing::instrument(skip(self, user_ids), fields(group_id = %group_id, users = user_ids.len()))]
    async fn delete_members(
        &self,
        group_id: &str,
        user_ids: &[String],
    ) -> Result<Vec<GroupMember>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT groupid AS "groupid!", userid AS "userid!",
                   COALESCE(createat, 0) AS "createat!", COALESCE(deleteat, 0) AS "deleteat!"
              FROM groupmembers
             WHERE groupid = $1 AND userid = ANY($2) AND deleteat = 0
            "#,
            group_id,
            user_ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to select GroupMembers".to_owned(),
            source,
        })?;

        let mut members: Vec<GroupMember> = rows
            .into_iter()
            .map(|row| GroupMember {
                group_id: row.groupid,
                user_id: row.userid,
                create_at: row.createat,
                delete_at: row.deleteat,
            })
            .collect();

        // Go compares lengths first, so a request that names one member twice passes — both
        // copies are "retrieved" — while a single id with no active row is refused.
        if members.len() != user_ids.len() {
            for user_id in user_ids {
                if !members.iter().any(|m| &m.user_id == user_id) {
                    return Err(StoreError::NotFound {
                        entity: "User",
                        criteria: user_id.clone(),
                    });
                }
            }
        }

        let delete_at = get_millis();
        for member in &mut members {
            member.delete_at = delete_at;
        }

        // No `DeleteAt = 0` predicate on the update, exactly as in Go — every named row is
        // stamped, including one deleted earlier, which the select above did not return.
        sqlx::query!(
            "UPDATE groupmembers SET deleteat = $1 WHERE groupid = $2 AND userid = ANY($3)",
            delete_at,
            group_id,
            user_ids
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to delete GroupMembers".to_owned(),
            source,
        })?;
        Ok(members)
    }

    #[tracing::instrument(skip(self, opts, view_restrictions), fields(page, per_page, found))]
    async fn get_groups(
        &self,
        page: i64,
        per_page: i64,
        opts: &GroupSearchOpts,
        view_restrictions: Option<&ViewUsersRestrictions>,
    ) -> Result<Vec<Group>, StoreError> {
        // Go assembles this statement a clause at a time from `opts`; this is the same statement
        // with every optional clause guarded by the flag that would have added it, so it stays
        // compile-checked. The three places the shape differs, and why they are the same set:
        //
        //   - The `Members` and `ChannelMembers` subqueries are joined unconditionally. Each is
        //     `GROUP BY GroupId`, so a LEFT JOIN onto it adds no rows; when its option is off
        //     the column is dropped in the mapping below, and when `include_channel_member_count`
        //     is `""` the subquery is empty. Go's timezone-off variant also omits the `Users`
        //     join inside the channel subquery; joining it changes nothing because the join is
        //     LEFT and one-to-one, and `COUNT(ChannelMembers.UserId)` never sees it.
        //   - `filter_has_member` is Go's `LEFT JOIN GroupMembers … WHERE UserId = ? AND
        //     DeleteAt = 0`, which is an `EXISTS` in all but spelling: `(GroupId, UserId)` is
        //     the table's primary key, so the join matches at most one row per group and cannot
        //     multiply them.
        //   - `LIMIT NULL OFFSET NULL` is "no limit" in Postgres, which is Go's `perPage == 0`.
        //     `wrapping_mul` for the offset, as the members-minus-group-members reads do.
        //
        // The `DeleteAt` predicate is Go's three-way `if`: `FilterArchived` wins, otherwise
        // `DeleteAt = 0` **unless** `IncludeArchived` or a positive `Since` ("mobile needs to
        // return archived groups when the since parameter is set").
        let only_archived = opts.filter_archived;
        let only_live = !opts.filter_archived && !opts.include_archived && opts.since <= 0;
        let pattern = like_pattern(&opts.q);
        // `len(opts.NotAssociatedToTeam) == 26` — a length check, not `IsValidId`.
        let not_team = opts.not_associated_to_team.len() == 26;
        let not_channel = opts.not_associated_to_channel.len() == 26;
        let parent_permitted = opts.filter_parent_team_permitted && not_channel;
        // `if opts.Source != "" { … } else if opts.OnlySyncableSources { … }`: the syncable
        // filter is reached only with no explicit source.
        let only_syncable = opts.source.as_str().is_empty() && opts.only_syncable_sources;
        let syncable_sources: Vec<String> = get_syncable_group_sources()
            .into_iter()
            .map(|s| s.0)
            .collect();
        // `sq.Like{"UserGroups.Source": prefix + "%"}` — `plugin_%`, in which the underscore is
        // LIKE's single-character wildcard. Reproduced, not escaped: Go matches `pluginXfoo`.
        let syncable_patterns: Vec<String> = get_syncable_group_source_prefixes()
            .into_iter()
            .map(|p| format!("{}%", p.0))
            .collect();
        // `applyViewRestrictionsFilter(countQuery, viewRestrictions, false)`: two INNER JOINs
        // under a `COUNT(DISTINCT Users.Id)`, which is what makes an `EXISTS` per list the same
        // count. Go's `1 = 0` for "non-nil and both empty" is the shape `GetViewUsersRestrictions`
        // (app/user.go:2756) always builds — `[]string{}`, never nil — so `Some` with two empty
        // lists counts nobody, and `None` is Go's nil.
        let (restricted_to_nobody, teams, channels) = match view_restrictions {
            None => (false, Vec::new(), Vec::new()),
            Some(r) => (
                r.teams.is_empty() && r.channels.is_empty(),
                r.teams.clone(), // bound as a `text[]` parameter, which sqlx takes by value
                r.channels.clone(),
            ),
        };
        let limit: Option<i64> = (per_page != 0).then_some(per_page);
        let offset: Option<i64> = (per_page != 0).then_some(page.wrapping_mul(per_page));

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
                   ug.allowreference,
                   COALESCE(members.membercount, 0) AS "membercount!",
                   COALESCE(channelmembers.channelmembercount, 0) AS "channelmembercount!",
                   COALESCE(channelmembers.channelmembertimezonescount, 0)
                       AS "channelmembertimezonescount!"
              FROM usergroups ug
              LEFT JOIN (
                    SELECT gm.groupid, COUNT(DISTINCT u.id) AS membercount
                      FROM groupmembers gm
                      LEFT JOIN users u ON u.id = gm.userid
                     WHERE gm.deleteat = 0
                       AND u.deleteat = 0
                       AND NOT $1::boolean
                       AND (cardinality($2::text[]) = 0 OR EXISTS (
                             SELECT 1 FROM teammembers rtm
                              WHERE rtm.userid = u.id
                                AND rtm.deleteat = 0
                                AND rtm.teamid = ANY($2::text[])))
                       AND (cardinality($3::text[]) = 0 OR EXISTS (
                             SELECT 1 FROM channelmembers rcm
                              WHERE rcm.userid = u.id
                                AND rcm.channelid = ANY($3::text[])))
                     GROUP BY gm.groupid
                   ) AS members ON members.groupid = ug.id
              LEFT JOIN (
                    SELECT gm.groupid,
                           COUNT(cm.userid) AS channelmembercount,
                           COUNT(DISTINCT (
                               CASE WHEN u.timezone->>'useAutomaticTimezone' = 'true'
                                     AND length(u.timezone->>'automaticTimezone') > 0
                                    THEN u.timezone->>'automaticTimezone'
                                    WHEN u.timezone->>'useAutomaticTimezone' = 'false'
                                     AND length(u.timezone->>'manualTimezone') > 0
                                    THEN u.timezone->>'manualTimezone'
                               END)) AS channelmembertimezonescount
                      FROM channelmembers cm
                      LEFT JOIN groupmembers gm ON gm.userid = cm.userid AND gm.deleteat = 0
                      LEFT JOIN users u ON u.id = gm.userid
                     WHERE cm.channelid = $4::text
                     GROUP BY gm.groupid
                   ) AS channelmembers ON channelmembers.groupid = ug.id
             WHERE ($5::text = '' OR EXISTS (
                       SELECT 1 FROM groupmembers hm
                        WHERE hm.groupid = ug.id AND hm.userid = $5::text AND hm.deleteat = 0))
               AND ($6::bigint <= 0 OR ug.updateat > $6::bigint)
               AND (NOT $7::boolean OR ug.deleteat > 0)
               AND (NOT $8::boolean OR ug.deleteat = 0)
               AND (NOT $9::boolean OR ug.allowreference = TRUE)
               AND ($10::text = '' OR ug.name ILIKE $11::text OR ug.displayname ILIKE $11::text)
               AND (NOT $12::boolean OR ug.id NOT IN (
                       SELECT iug.id
                         FROM usergroups iug
                         JOIN groupteams gt ON gt.groupid = iug.id
                        WHERE gt.deleteat = 0
                          AND iug.deleteat = 0
                          AND gt.teamid = $13::text))
               AND (NOT $14::boolean OR ug.id NOT IN (
                       SELECT iug.id
                         FROM usergroups iug
                         JOIN groupchannels gc ON gc.groupid = iug.id
                        WHERE gc.deleteat = 0
                          AND iug.deleteat = 0
                          AND gc.channelid = $15::text))
               AND (NOT $16::boolean OR CASE
                       WHEN (SELECT t.groupconstrained
                               FROM teams t
                               JOIN channels c ON c.teamid = t.id
                              WHERE c.id = $15::text)
                       THEN ug.id IN (
                            SELECT gt.groupid
                              FROM groupteams gt
                             WHERE gt.deleteat = 0
                               AND gt.teamid = (SELECT c.teamid FROM channels c WHERE c.id = $15::text))
                       ELSE TRUE
                    END)
               AND ($17::text = '' OR ug.source = $17::text)
               AND (NOT $18::boolean
                    OR ug.source = ANY($19::text[])
                    OR ug.source LIKE ANY($20::text[]))
             ORDER BY CASE WHEN NOT $21::boolean OR ug.deleteat = 0 THEN ug.displayname END,
                      CASE WHEN $21::boolean AND ug.deleteat <> 0 THEN ug.displayname END
             LIMIT $22::bigint OFFSET $23::bigint
            "#,
            restricted_to_nobody,
            &teams,
            &channels,
            opts.include_channel_member_count,
            opts.filter_has_member,
            opts.since,
            only_archived,
            only_live,
            opts.filter_allow_reference,
            opts.q,
            pattern,
            not_team,
            opts.not_associated_to_team,
            not_channel,
            opts.not_associated_to_channel,
            parent_permitted,
            opts.source.as_str(),
            only_syncable,
            &syncable_sources,
            &syncable_patterns,
            opts.include_archived,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Groups".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        let with_channel_count = !opts.include_channel_member_count.is_empty();
        Ok(rows
            .into_iter()
            .map(|row| {
                let mut group = GroupRow {
                    id: row.id,
                    name: row.name,
                    displayname: row.displayname,
                    description: row.description,
                    source: row.source,
                    remoteid: row.remoteid,
                    createat: row.createat,
                    updateat: row.updateat,
                    deleteat: row.deleteat,
                    allowreference: row.allowreference,
                }
                .into_group();
                group.member_count = opts.include_member_count.then_some(row.membercount);
                group.channel_member_count = with_channel_count.then_some(row.channelmembercount);
                group.channel_member_timezones_count = (with_channel_count
                    && opts.include_timezones)
                    .then_some(row.channelmembertimezonescount);
                group
            })
            .collect())
    }

    #[tracing::instrument(skip(self, opts), fields(channel_id = %channel_id, found))]
    async fn get_groups_by_channel(
        &self,
        channel_id: &str,
        opts: &GroupSearchOpts,
    ) -> Result<Vec<GroupWithSchemeAdmin>, StoreError> {
        // `groupsBySyncableBaseQuery(GroupSyncableTypeChannel, selectGroups, …)`
        // (group_store.go:1309), then the `PageOpts` block of `GetGroupsByChannel`.
        //
        // The `Members` subquery is Go's `COUNT(*)` over live memberships of live users, joined
        // unconditionally for the reason `get_groups` gives, and dropped from the model when
        // `include_member_count` is off. The `ORDER BY` is conditional in Go twice over —
        // `IncludeMemberCount` adds one and `PageOpts` adds the same one again — so it is
        // applied when either is set and **absent otherwise**, as the CASE with a null key is.
        let ordered = opts.include_member_count || opts.page_opts.is_some();
        let pattern = like_pattern(&opts.q);
        let limit: Option<i64> = opts.page_opts.map(|p| p.per_page);
        let offset: Option<i64> = opts.page_opts.map(|p| p.page.wrapping_mul(p.per_page));
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
                   ug.allowreference,
                   gs.schemeadmin AS syncableschemeadmin,
                   COALESCE(members.membercount, 0) AS "membercount!"
              FROM usergroups ug
              JOIN groupchannels gs ON gs.groupid = ug.id
              LEFT JOIN (
                    SELECT gm.groupid, COUNT(*) AS membercount
                      FROM groupmembers gm
                      LEFT JOIN users u ON u.id = gm.userid
                     WHERE gm.deleteat = 0 AND u.deleteat = 0
                     GROUP BY gm.groupid
                   ) AS members ON members.groupid = ug.id
             WHERE gs.channelid = $1::text
               AND gs.deleteat = 0
               AND ug.deleteat = 0
               AND (NOT $2::boolean OR ug.allowreference = TRUE)
               AND ($3::text = '' OR ug.name ILIKE $4::text OR ug.displayname ILIKE $4::text)
             ORDER BY CASE WHEN $5::boolean THEN ug.displayname END
             LIMIT $6::bigint OFFSET $7::bigint
            "#,
            channel_id,
            opts.filter_allow_reference,
            opts.q,
            pattern,
            ordered,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Groups with channelId={channel_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows
            .into_iter()
            .map(|row| {
                with_scheme_admin(
                    GroupRow {
                        id: row.id,
                        name: row.name,
                        displayname: row.displayname,
                        description: row.description,
                        source: row.source,
                        remoteid: row.remoteid,
                        createat: row.createat,
                        updateat: row.updateat,
                        deleteat: row.deleteat,
                        allowreference: row.allowreference,
                    },
                    row.syncableschemeadmin,
                    opts.include_member_count.then_some(row.membercount),
                )
            })
            .collect())
    }

    #[tracing::instrument(skip(self, opts), fields(team_id = %team_id, found))]
    async fn get_groups_by_team(
        &self,
        team_id: &str,
        opts: &GroupSearchOpts,
    ) -> Result<Vec<GroupWithSchemeAdmin>, StoreError> {
        // The channel query with `GroupTeams`/`TeamId` substituted — the `st ==
        // GroupSyncableTypeTeam` arm of `groupsBySyncableBaseQuery`. Everything the channel
        // method documents holds here unchanged.
        let ordered = opts.include_member_count || opts.page_opts.is_some();
        let pattern = like_pattern(&opts.q);
        let limit: Option<i64> = opts.page_opts.map(|p| p.per_page);
        let offset: Option<i64> = opts.page_opts.map(|p| p.page.wrapping_mul(p.per_page));
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
                   ug.allowreference,
                   gs.schemeadmin AS syncableschemeadmin,
                   COALESCE(members.membercount, 0) AS "membercount!"
              FROM usergroups ug
              JOIN groupteams gs ON gs.groupid = ug.id
              LEFT JOIN (
                    SELECT gm.groupid, COUNT(*) AS membercount
                      FROM groupmembers gm
                      LEFT JOIN users u ON u.id = gm.userid
                     WHERE gm.deleteat = 0 AND u.deleteat = 0
                     GROUP BY gm.groupid
                   ) AS members ON members.groupid = ug.id
             WHERE gs.teamid = $1::text
               AND gs.deleteat = 0
               AND ug.deleteat = 0
               AND (NOT $2::boolean OR ug.allowreference = TRUE)
               AND ($3::text = '' OR ug.name ILIKE $4::text OR ug.displayname ILIKE $4::text)
             ORDER BY CASE WHEN $5::boolean THEN ug.displayname END
             LIMIT $6::bigint OFFSET $7::bigint
            "#,
            team_id,
            opts.filter_allow_reference,
            opts.q,
            pattern,
            ordered,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Groups with teamId={team_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows
            .into_iter()
            .map(|row| {
                with_scheme_admin(
                    GroupRow {
                        id: row.id,
                        name: row.name,
                        displayname: row.displayname,
                        description: row.description,
                        source: row.source,
                        remoteid: row.remoteid,
                        createat: row.createat,
                        updateat: row.updateat,
                        deleteat: row.deleteat,
                        allowreference: row.allowreference,
                    },
                    row.syncableschemeadmin,
                    opts.include_member_count.then_some(row.membercount),
                )
            })
            .collect())
    }
}

impl SqlGroupStore {
    /// Port of `SqlGroupStore.checkUsersExist` (group_store.go:218): every id must name an
    /// **active** user, and the first missing one — in request order — is the error.
    async fn check_users_exist(&self, user_ids: &[String]) -> Result<(), StoreError> {
        if user_ids.is_empty() {
            return Ok(());
        }
        let found = sqlx::query_scalar!(
            r#"SELECT id AS "id!" FROM users WHERE id = ANY($1) AND deleteat = 0"#,
            user_ids
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to check Users exist".to_owned(),
            source,
        })?;
        if found.len() == user_ids.len() {
            return Ok(());
        }
        for user_id in user_ids {
            if !found.contains(user_id) {
                return Err(StoreError::NotFound {
                    entity: "User",
                    criteria: user_id.clone(),
                });
            }
        }
        Ok(())
    }
}

/// The ten stored columns of `UserGroups` (`userGroupsSelectQuery`, group_store.go:62).
struct GroupRow {
    id: String,
    name: Option<String>,
    displayname: Option<String>,
    description: Option<String>,
    source: Option<String>,
    remoteid: Option<String>,
    createat: Option<i64>,
    updateat: Option<i64>,
    deleteat: Option<i64>,
    allowreference: Option<bool>,
}

impl GroupRow {
    /// `group.ToModel()` (group_store.go:1155) for a row read without the joined counts: the
    /// five `db:"-"` fields stay at their zero values, as Go's scan leaves them.
    fn into_group(self) -> Group {
        Group {
            id: self.id,
            name: self.name,
            display_name: self.displayname.unwrap_or_default(),
            description: self.description.unwrap_or_default(),
            source: GroupSource(self.source.unwrap_or_default()),
            remote_id: self.remoteid,
            create_at: self.createat.unwrap_or_default(),
            update_at: self.updateat.unwrap_or_default(),
            delete_at: self.deleteat.unwrap_or_default(),
            allow_reference: self.allowreference.unwrap_or_default(),
            has_syncables: false,
            member_count: None,
            channel_member_count: None,
            channel_member_timezones_count: None,
            member_ids: None,
        }
    }
}

/// `groupWithSchemeAdmin.ToModel()` (group_store.go:1189): a null `SyncableSchemeAdmin` becomes
/// `false`, so the field is never absent on the wire for these two reads.
fn with_scheme_admin(
    row: GroupRow,
    scheme_admin: Option<bool>,
    member_count: Option<i64>,
) -> GroupWithSchemeAdmin {
    let mut group = row.into_group();
    group.member_count = member_count;
    GroupWithSchemeAdmin {
        group,
        scheme_admin: Some(scheme_admin.unwrap_or(false)),
    }
}

/// The `%<term>%` every group search builds: `sanitizeSearchTerm(opts.Q, "\\")` strips the
/// escape character and escapes `%` and `_`, then `fmt.Sprintf("%%%s%%", …)` wraps it. An empty
/// `q` produces `%%`, which is never reached — every caller guards on `q != ""` first.
fn like_pattern(q: &str) -> String {
    format!("%{}%", sanitize_search_term(q, '\\'))
}

/// `IsUniqueConstraintError(err, []string{"Name", "groups_name_key"})` (group_store.go:172):
/// the `UNIQUE(name)` of `000007_create_user_groups.up.sql`, which Postgres names
/// `usergroups_name_key`.
fn is_group_name_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db| db.constraint())
        .is_some_and(|constraint| constraint.contains("groups_name_key"))
}
