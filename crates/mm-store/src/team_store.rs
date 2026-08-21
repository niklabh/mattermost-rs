//! Port of `SqlTeamStore` (channels/store/sqlstore/team_store.go): the team reads (`Get`,
//! `GetByName`, `GetTeamsByUserId`), the membership reads (`GetTeamsForUser`, `GetMember`,
//! `GetMembers`), the two member counts, the per-channel unread rows behind the team unread
//! badges (`GetChannelUnreadsForAllTeams`), and the scheme-roles machinery they exist to drive.
//!
//! # Why this is not just a SELECT
//!
//! A team member's **effective** roles are not what the `Roles` column says. They are computed
//! from three booleans on `TeamMembers`, three nullable role names on the team's `Scheme`, and
//! whatever is in `Roles` — because roles that predate the scheme migration are still sitting in
//! that column and have to be recognised and moved. `getTeamRoles` (team_store.go:100) is that
//! computation, and getting it wrong produces a **silent** permission difference rather than an
//! error: a member who should be a team admin quietly is not, or vice versa.

use mm_model::channel_member::ChannelUnread;
use mm_model::team::Team;
use mm_model::team_member::TeamMember;
use mm_model::utils::StringMap;
use sqlx::PgPool;

use crate::error::StoreError;

/// `model.TeamGuestRoleId` (role.go:392).
pub const TEAM_GUEST_ROLE_ID: &str = "team_guest";
/// `model.TeamUserRoleId` (role.go:393).
pub const TEAM_USER_ROLE_ID: &str = "team_user";
/// `model.TeamAdminRoleId` (role.go:394).
pub const TEAM_ADMIN_ROLE_ID: &str = "team_admin";

/// Port of the unexported `rolesInfo` (team_store.go:92).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RolesInfo {
    pub roles: Vec<String>,
    pub explicit_roles: Vec<String>,
    pub scheme_guest: bool,
    pub scheme_user: bool,
    pub scheme_admin: bool,
}

/// The ported half of `model.TeamMembersGetOptions` (team_member.go:78) — `ViewRestrictions`
/// is dropped, see [`TeamStore::get_members`].
///
/// `sort` is Go's raw query value, not an enum, because Go's three-way branch keys on the
/// string: `""` orders by `UserId`, [`mm_model::team_member::USERNAME`] orders by `Username`,
/// and **anything else orders by nothing at all** — the heap order, whatever it is. An enum with
/// two variants would have to fold the third case into one of the others and change a result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamMembersGetOptions {
    pub sort: String,
    pub exclude_deleted_users: bool,
}

/// Port of `getTeamRoles` (team_store.go:100).
///
/// Two passes, and the order of both is on the wire because the result is joined with spaces.
///
/// 1. **Split `Roles`.** A role matching one of the three scheme role ids sets the corresponding
///    scheme flag — *even when the column said false* — and is dropped from both outputs. This is
///    the un-migrated case Go's comment describes. Anything else lands in `explicit_roles` **and**
///    `roles`, in the order it appeared.
/// 2. **Append the implied roles**, guest then user then admin, each being the scheme's default
///    role name when the team has a scheme and the constant otherwise, and each skipped if it is
///    already present in `roles`.
///
/// The dedup check reads `result.roles` as it grows, so two scheme defaults that happen to be the
/// same string collapse to one — that is Go's behaviour, not an accident of this port.
pub fn get_team_roles(
    scheme_guest: bool,
    scheme_user: bool,
    scheme_admin: bool,
    default_team_guest_role: &str,
    default_team_user_role: &str,
    default_team_admin_role: &str,
    roles: &str,
) -> RolesInfo {
    let mut result = RolesInfo {
        roles: Vec::new(),
        explicit_roles: Vec::new(),
        scheme_guest,
        scheme_user,
        scheme_admin,
    };

    // Go's `strings.Fields`: split on runs of whitespace, dropping empties. Rust's
    // `split_whitespace` is the same Unicode White_Space property Go's `unicode.IsSpace`
    // consults, so an empty or all-blank column yields no roles on both sides.
    for role in roles.split_whitespace() {
        match role {
            TEAM_GUEST_ROLE_ID => result.scheme_guest = true,
            TEAM_USER_ROLE_ID => result.scheme_user = true,
            TEAM_ADMIN_ROLE_ID => result.scheme_admin = true,
            other => {
                result.explicit_roles.push(other.to_owned());
                result.roles.push(other.to_owned());
            }
        }
    }

    // Scheme-implied roles, in Go's order: guest, user, admin.
    let mut implied: Vec<&str> = Vec::new();
    if result.scheme_guest {
        implied.push(if default_team_guest_role.is_empty() {
            TEAM_GUEST_ROLE_ID
        } else {
            default_team_guest_role
        });
    }
    if result.scheme_user {
        implied.push(if default_team_user_role.is_empty() {
            TEAM_USER_ROLE_ID
        } else {
            default_team_user_role
        });
    }
    if result.scheme_admin {
        implied.push(if default_team_admin_role.is_empty() {
            TEAM_ADMIN_ROLE_ID
        } else {
            default_team_admin_role
        });
    }

    for implied_role in implied {
        if !result.roles.iter().any(|role| role == implied_role) {
            result.roles.push(implied_role.to_owned());
        }
    }

    result
}

/// The subset of Go's `store.TeamStore` (store/store.go:135-199) that is ported.
pub trait TeamStore {
    /// Port of `SqlTeamStore.GetTeamsForUser` (team_store.go:1181).
    fn get_teams_for_user(
        &self,
        user_id: &str,
        exclude_team_id: &str,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<Vec<TeamMember>, StoreError>> + Send;

    /// Port of `SqlTeamStore.GetTeamsByUserId` (team_store.go:705) — the **teams**, where
    /// `get_teams_for_user` returns the memberships.
    fn get_teams_by_user_id(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Team>, StoreError>> + Send;

    /// Port of `SqlTeamStore.Get` (team_store.go:354).
    fn get(&self, id: &str) -> impl std::future::Future<Output = Result<Team, StoreError>> + Send;

    /// Port of `SqlTeamStore.GetByName` (team_store.go:424).
    fn get_by_name(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<Team, StoreError>> + Send;

    /// Port of `SqlTeamStore.GetMember` (team_store.go:1034).
    fn get_member(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<TeamMember, StoreError>> + Send;

    /// Port of `SqlTeamStore.GetMembers` (team_store.go:1063), restrictions-free — the
    /// `ViewRestrictions` half of `TeamMembersGetOptions` is dropped for the same reason as
    /// [`TeamStore::get_total_member_count`]'s parameter: the one route calling this forwards
    /// any restricted caller to Go.
    fn get_members(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
        options: &TeamMembersGetOptions,
    ) -> impl std::future::Future<Output = Result<Vec<TeamMember>, StoreError>> + Send;

    /// Port of `SqlTeamStore.GetTotalMemberCount` (team_store.go:1106), restrictions-free.
    ///
    /// Go's second parameter is a `*model.ViewUsersRestrictions` that splices extra joins into
    /// the query. It is dropped here rather than accepted and ignored: the one route that calls
    /// this (`getTeamStats`) **forwards to Go** whenever the caller's restrictions would be
    /// non-nil, so no caller of this port can ever hold one — same reasoning as the dropped
    /// `allowFromCache` parameters.
    fn get_total_member_count(
        &self,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlTeamStore.GetActiveMemberCount` (team_store.go:1130), restrictions-free —
    /// see [`TeamStore::get_total_member_count`].
    fn get_active_member_count(
        &self,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlTeamStore.GetChannelUnreadsForAllTeams` (team_store.go:1231).
    fn get_channel_unreads_for_all_teams(
        &self,
        exclude_team_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ChannelUnread>, StoreError>> + Send;

    /// Port of `SqlTeamStore.GetChannelUnreadsForTeam` (team_store.go:1253).
    fn get_channel_unreads_for_team(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ChannelUnread>, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlTeamStore {
    pool: PgPool,
}

impl SqlTeamStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl TeamStore for SqlTeamStore {
    #[tracing::instrument(skip_all, fields(user_id = %user_id, found))]
    async fn get_teams_for_user(
        &self,
        user_id: &str,
        exclude_team_id: &str,
        include_deleted: bool,
    ) -> Result<Vec<TeamMember>, StoreError> {
        get_teams_for_user(&self.pool, user_id, exclude_team_id, include_deleted).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, found))]
    async fn get_teams_by_user_id(&self, user_id: &str) -> Result<Vec<Team>, StoreError> {
        get_teams_by_user_id(&self.pool, user_id).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %id, found))]
    async fn get(&self, id: &str) -> Result<Team, StoreError> {
        get(&self.pool, id).await
    }

    #[tracing::instrument(skip_all, fields(name = %name, found))]
    async fn get_by_name(&self, name: &str) -> Result<Team, StoreError> {
        get_by_name(&self.pool, name).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, found))]
    async fn get_member(&self, team_id: &str, user_id: &str) -> Result<TeamMember, StoreError> {
        get_member(&self.pool, team_id, user_id).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, offset, limit, found))]
    async fn get_members(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
        options: &TeamMembersGetOptions,
    ) -> Result<Vec<TeamMember>, StoreError> {
        get_members(&self.pool, team_id, offset, limit, options).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id))]
    async fn get_total_member_count(&self, team_id: &str) -> Result<i64, StoreError> {
        get_total_member_count(&self.pool, team_id).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id))]
    async fn get_active_member_count(&self, team_id: &str) -> Result<i64, StoreError> {
        get_active_member_count(&self.pool, team_id).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, exclude_team_id = %exclude_team_id, found))]
    async fn get_channel_unreads_for_all_teams(
        &self,
        exclude_team_id: &str,
        user_id: &str,
    ) -> Result<Vec<ChannelUnread>, StoreError> {
        get_channel_unreads_for_all_teams(&self.pool, exclude_team_id, user_id).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id, found))]
    async fn get_channel_unreads_for_team(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> Result<Vec<ChannelUnread>, StoreError> {
        get_channel_unreads_for_team(&self.pool, team_id, user_id).await
    }
}

/// Port of `SqlTeamStore.GetTotalMemberCount` (team_store.go:1106), restrictions-free — see the
/// trait method for why the parameter is dropped.
///
/// "Total" is **current memberships including deactivated users**: the membership's own
/// `DeleteAt = 0` filters departures, and there is deliberately no `Users.DeleteAt` predicate —
/// that one extra predicate is the entire difference from [`get_active_member_count`]. A
/// soft-deleted membership row therefore counts in *neither* number, while a deactivated user's
/// surviving row counts in this one only.
#[tracing::instrument(skip(pool), fields(team_id = %team_id))]
pub async fn get_total_member_count(pool: &PgPool, team_id: &str) -> Result<i64, StoreError> {
    sqlx::query_scalar!(
        r#"
        SELECT count(DISTINCT teammembers.userid) AS "count!"
          FROM teammembers, users
         WHERE teammembers.deleteat = 0
           AND teammembers.userid = users.id
           AND teammembers.teamid = $1
        "#,
        team_id
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to count TeamMembers".to_owned(),
        source,
    })
}

/// Port of `SqlTeamStore.GetActiveMemberCount` (team_store.go:1130), restrictions-free.
///
/// [`get_total_member_count`]'s query plus `Users.DeleteAt = 0`. Go's error context is the same
/// string in both functions ("failed to count TeamMembers"), reproduced rather than improved.
#[tracing::instrument(skip(pool), fields(team_id = %team_id))]
pub async fn get_active_member_count(pool: &PgPool, team_id: &str) -> Result<i64, StoreError> {
    sqlx::query_scalar!(
        r#"
        SELECT count(DISTINCT teammembers.userid) AS "count!"
          FROM teammembers, users
         WHERE teammembers.deleteat = 0
           AND teammembers.userid = users.id
           AND users.deleteat = 0
           AND teammembers.teamid = $1
        "#,
        team_id
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to count TeamMembers".to_owned(),
        source,
    })
}

/// The row shape of Go's `teamSliceColumns(true)` (team_store.go:40): every `Teams` column the
/// store selects plus the two access-control flags computed per row. Shared by [`get`],
/// [`get_by_name`] and [`get_teams_by_user_id`] so the three queries cannot drift in what they
/// select or how a row becomes a [`Team`] — the same lift as `channel_store::ChannelRow`.
struct TeamRow {
    id: String,
    createat: Option<i64>,
    updateat: Option<i64>,
    deleteat: Option<i64>,
    displayname: Option<String>,
    name: Option<String>,
    description: Option<String>,
    email: Option<String>,
    team_type: Option<String>,
    companyname: Option<String>,
    alloweddomains: Option<String>,
    inviteid: Option<String>,
    allowopeninvite: Option<bool>,
    lastteamiconupdate: Option<i64>,
    schemeid: Option<String>,
    groupconstrained: Option<bool>,
    cloudlimitsarchived: bool,
    policy_enforced: bool,
    policy_is_active: bool,
}

/// Go's `sqlx.Get` into `model.Team`: every nullable column reads as its zero value.
fn team_from_row(row: TeamRow) -> Team {
    Team {
        id: row.id,
        create_at: row.createat.unwrap_or_default(),
        update_at: row.updateat.unwrap_or_default(),
        delete_at: row.deleteat.unwrap_or_default(),
        display_name: row.displayname.unwrap_or_default(),
        name: row.name.unwrap_or_default(),
        description: row.description.unwrap_or_default(),
        email: row.email.unwrap_or_default(),
        team_type: row.team_type.unwrap_or_default(),
        company_name: row.companyname.unwrap_or_default(),
        allowed_domains: row.alloweddomains.unwrap_or_default(),
        invite_id: row.inviteid.unwrap_or_default(),
        allow_open_invite: row.allowopeninvite.unwrap_or_default(),
        last_team_icon_update: row.lastteamiconupdate.unwrap_or_default(),
        scheme_id: row.schemeid,
        group_constrained: row.groupconstrained,
        cloud_limits_archived: row.cloudlimitsarchived,
        policy_enforced: row.policy_enforced,
        policy_is_active: row.policy_is_active,

        // Not selected by Go's `teamSliceColumns`; each is filled elsewhere or left zero.
        policy_id: None,
        policy_actions: None,
        recommended: false,
    }
}

/// Port of `SqlTeamStore.Get` (team_store.go:354).
///
/// The one row by primary key, with **no `DeleteAt` filter** — an archived team still answers,
/// exactly like `SqlChannelStore.Get`, and unlike `GetTeamsByUserId` above whose team-side
/// `DeleteAt = 0` removes archived teams from a user's list. The same team can therefore be
/// missing from `GET /users/{id}/teams` and served by `GET /teams/{id}`, and both answers are
/// Go's — the same asymmetry the channel routes already pinned.
///
/// The columns are `teamSliceColumns(true)` — identical to [`get_teams_by_user_id`]'s SELECT,
/// including the two computed `AccessControlPolicies` flags with their `Type = 'team'` guard.
///
/// Go follows the fetch with `if team.Id == "" { return ErrNotFound }` (team_store.go:365). By
/// primary-key semantics a found row's `Id` equals the parameter, so that guard can only fire
/// for `id = ""` — which the fetch already misses. Ported anyway: it is two lines, and dropping
/// a branch because *we* reasoned it dead is how [D-151]-class drift starts.
#[tracing::instrument(skip(pool), fields(team_id = %id))]
pub async fn get(pool: &PgPool, id: &str) -> Result<Team, StoreError> {
    let row = sqlx::query_as!(
        TeamRow,
        r#"
        SELECT t.id,
               t.createat,
               t.updateat,
               t.deleteat,
               t.displayname,
               t.name,
               t.description,
               t.email,
               t.type::text AS "team_type",
               t.companyname,
               t.alloweddomains,
               t.inviteid,
               t.allowopeninvite,
               t.lastteamiconupdate,
               t.schemeid,
               t.groupconstrained,
               t.cloudlimitsarchived,
               EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp
                    WHERE acp.id = t.id AND acp.type = 'team'
               ) AS "policy_enforced!",
               COALESCE((
                   SELECT acp.active FROM accesscontrolpolicies acp
                    WHERE acp.id = t.id AND acp.type = 'team' AND acp.active = TRUE
                    LIMIT 1
               ), false) AS "policy_is_active!"
          FROM teams t
         WHERE t.id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get Team with id={id}"),
        source,
    })?;

    let Some(row) = row else {
        tracing::Span::current().record("found", false);
        return Err(StoreError::NotFound {
            entity: "Team",
            criteria: id.to_owned(),
        });
    };

    // Go's `team.Id == ""` guard (team_store.go:365) — see the doc comment.
    if row.id.is_empty() {
        tracing::Span::current().record("found", false);
        return Err(StoreError::NotFound {
            entity: "Team",
            criteria: id.to_owned(),
        });
    }
    tracing::Span::current().record("found", true);

    Ok(team_from_row(row))
}

/// Port of `SqlTeamStore.GetByName` (team_store.go:424).
///
/// [`get`]'s query with `Name = $1` for `Id = $1`, and the same absence of a `DeleteAt` filter —
/// an archived team still answers by name. **No case folding**: `sq.Eq{"Name": name}` is an
/// exact match, unlike `GetByUsername`'s `lower(?)`, and the handler's `IsValidTeamName` rejects
/// anything with an uppercase letter before the query could see it anyway. No `Id == ""` guard
/// either — that is `Get`'s alone (team_store.go:365 versus :435).
///
/// `Teams.Name` is unique, so `fetch_optional` is Go's `sqlx.Get`: the not-found criteria
/// string is `name=<name>`, where `Get`'s is the bare id.
#[tracing::instrument(skip(pool), fields(name = %name))]
pub async fn get_by_name(pool: &PgPool, name: &str) -> Result<Team, StoreError> {
    let row = sqlx::query_as!(
        TeamRow,
        r#"
        SELECT t.id,
               t.createat,
               t.updateat,
               t.deleteat,
               t.displayname,
               t.name,
               t.description,
               t.email,
               t.type::text AS "team_type",
               t.companyname,
               t.alloweddomains,
               t.inviteid,
               t.allowopeninvite,
               t.lastteamiconupdate,
               t.schemeid,
               t.groupconstrained,
               t.cloudlimitsarchived,
               EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp
                    WHERE acp.id = t.id AND acp.type = 'team'
               ) AS "policy_enforced!",
               COALESCE((
                   SELECT acp.active FROM accesscontrolpolicies acp
                    WHERE acp.id = t.id AND acp.type = 'team' AND acp.active = TRUE
                    LIMIT 1
               ), false) AS "policy_is_active!"
          FROM teams t
         WHERE t.name = $1
        "#,
        name
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find Team with name={name}"),
        source,
    })?;

    let Some(row) = row else {
        tracing::Span::current().record("found", false);
        return Err(StoreError::NotFound {
            entity: "Team",
            criteria: format!("name={name}"),
        });
    };
    tracing::Span::current().record("found", true);

    Ok(team_from_row(row))
}

/// Port of `SqlTeamStore.GetTeamsByUserId` (team_store.go:705).
///
/// Three predicates, all Go's (`sq.Eq{...}` over the `TeamMembers` join), and both `DeleteAt`s
/// matter separately: **the membership's** (a user removed from a team keeps a soft-deleted row)
/// and **the team's** (an archived team keeps its members' rows live). Dropping either
/// resurrects a different thing — a departed member or a dead team — and both read as "the user
/// has an extra team", so a fixture has to hold one of each to tell them apart.
///
/// The two `AccessControlPolicies` subqueries are `teamSliceColumns(true)`'s computed flags with
/// the `Type = 'team'` guard whose comment Go spells out: a channel policy sharing an id with a
/// team must not read as the team's. `Props`/`PolicyId`/`PolicyActions` are not selected by Go
/// either; they stay zero. No `ORDER BY` — the row order is whatever Postgres returns, and Go's
/// callers do not sort it, so neither does this port.
#[tracing::instrument(skip(pool), fields(user_id = %user_id))]
pub async fn get_teams_by_user_id(pool: &PgPool, user_id: &str) -> Result<Vec<Team>, StoreError> {
    let rows = sqlx::query_as!(
        TeamRow,
        r#"
        SELECT t.id,
               t.createat,
               t.updateat,
               t.deleteat,
               t.displayname,
               t.name,
               t.description,
               t.email,
               t.type::text AS "team_type",
               t.companyname,
               t.alloweddomains,
               t.inviteid,
               t.allowopeninvite,
               t.lastteamiconupdate,
               t.schemeid,
               t.groupconstrained,
               t.cloudlimitsarchived,
               EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp
                    WHERE acp.id = t.id AND acp.type = 'team'
               ) AS "policy_enforced!",
               COALESCE((
                   SELECT acp.active FROM accesscontrolpolicies acp
                    WHERE acp.id = t.id AND acp.type = 'team' AND acp.active = TRUE
                    LIMIT 1
               ), false) AS "policy_is_active!"
          FROM teams t
          JOIN teammembers tm ON tm.teamid = t.id
         WHERE tm.userid = $1
           AND tm.deleteat = 0
           AND t.deleteat = 0
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to find Teams".to_owned(),
        source,
    })?;

    Ok(rows.into_iter().map(team_from_row).collect())
}

/// The row shape of Go's `teamMemberWithSchemeRoles` (team_store.go:62): the `TeamMembers`
/// columns plus the team scheme's three default role names. Shared by every membership read so
/// `ToModel` is written once.
struct TeamMemberRow {
    teamid: String,
    userid: String,
    roles: Option<String>,
    deleteat: Option<i64>,
    schemeuser: Option<bool>,
    schemeadmin: Option<bool>,
    schemeguest: Option<bool>,
    createat: Option<i64>,
    defaultteamguestrole: Option<String>,
    defaultteamuserrole: Option<String>,
    defaultteamadminrole: Option<String>,
}

/// Port of `teamMemberWithSchemeRoles.ToModel` (team_store.go:162). Go's `sql.NullBool` and
/// `sql.NullString` both mean "NULL is the zero value" here — `Valid && Bool` for the flags,
/// `""` for the role names — so `unwrap_or_default` is the same rule, not a looser one.
fn team_member_from_row(row: TeamMemberRow) -> TeamMember {
    let roles_result = get_team_roles(
        row.schemeguest.unwrap_or_default(),
        row.schemeuser.unwrap_or_default(),
        row.schemeadmin.unwrap_or_default(),
        row.defaultteamguestrole.as_deref().unwrap_or_default(),
        row.defaultteamuserrole.as_deref().unwrap_or_default(),
        row.defaultteamadminrole.as_deref().unwrap_or_default(),
        row.roles.as_deref().unwrap_or_default(),
    );

    TeamMember {
        team_id: row.teamid,
        user_id: row.userid,
        roles: roles_result.roles.join(" "),
        delete_at: row.deleteat.unwrap_or_default(),
        scheme_guest: roles_result.scheme_guest,
        scheme_user: roles_result.scheme_user,
        scheme_admin: roles_result.scheme_admin,
        explicit_roles: roles_result.explicit_roles.join(" "),
        create_at: row.createat.unwrap_or_default(),
    }
}

/// Port of `SqlTeamStore.GetMember` (team_store.go:1034).
///
/// `getTeamMembersWithSchemeSelectQuery` — the same two LEFT JOINs as [`get_teams_for_user`] —
/// narrowed to one `(TeamId, UserId)` pair. **No `DeleteAt` filter**: a departed member's
/// soft-deleted row still answers, with its non-zero `delete_at` on the wire, where
/// [`get_members`] below filters it out. Go reads this one from the **master** (`GetMember` is
/// wrapped in `RequestContextWithMaster`); this port has one pool, so that is already true
/// ([D-140]).
#[tracing::instrument(skip(pool), fields(team_id = %team_id, user_id = %user_id))]
pub async fn get_member(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
) -> Result<TeamMember, StoreError> {
    let row = sqlx::query_as!(
        TeamMemberRow,
        r#"
        SELECT tm.teamid,
               tm.userid,
               tm.roles,
               tm.deleteat,
               tm.schemeuser,
               tm.schemeadmin,
               tm.schemeguest,
               tm.createat,
               ts.defaultteamguestrole,
               ts.defaultteamuserrole,
               ts.defaultteamadminrole
          FROM teammembers tm
          LEFT JOIN teams t ON tm.teamid = t.id
          LEFT JOIN schemes ts ON t.schemeid = ts.id
         WHERE tm.teamid = $1
           AND tm.userid = $2
        "#,
        team_id,
        user_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find TeamMembers with teamId={team_id} and userId={user_id}"),
        source,
    })?;

    let Some(row) = row else {
        tracing::Span::current().record("found", false);
        return Err(StoreError::NotFound {
            entity: "TeamMember",
            criteria: format!("teamId={team_id}, userId={user_id}"),
        });
    };
    tracing::Span::current().record("found", true);

    Ok(team_member_from_row(row))
}

/// Port of `SqlTeamStore.GetMembers` (team_store.go:1063), restrictions-free.
///
/// Four of Go's decisions ride along, and the first is the opposite of its channel twin:
///
/// - **`Limit`/`Offset` are unconditional here** — `.Limit(uint64(limit)).Offset(uint64(offset))`
///   with no `> 0` guard, so `limit = 0` is `LIMIT 0` and **`?per_page=0` is an empty list**,
///   where `SqlChannelStore.GetMembers` guards the clause and serves the whole channel. Same
///   parser, same zero, opposite answer; both measured against the running Go server.
/// - **The ordering is a three-way branch on the raw `sort` string.** `""` orders by `UserId`;
///   `"Username"` orders by `Users.Username`; **any other value orders by nothing** — Go skips
///   the `UserId` default because `Sort != ""` and skips the username one because
///   `Sort != USERNAME`. Expressed as two `CASE` keys so one statement stays compile-checked.
///   That third shape is heap order, which both servers share but neither promises.
/// - **`DeleteAt = 0` on the membership, always** — a departed member never appears in the
///   list, though [`get_member`] still serves the row singly.
/// - `exclude_deleted_users` adds `Users.DeleteAt = 0`. Go LEFT JOINs `Users` only when the
///   sort or the flag needs it; joining it unconditionally is result-equivalent (`Users.Id` is
///   the primary key, so the LEFT JOIN neither multiplies nor drops rows) and keeps one query.
#[tracing::instrument(skip(pool), fields(team_id = %team_id, offset, limit, found))]
pub async fn get_members(
    pool: &PgPool,
    team_id: &str,
    offset: i64,
    limit: i64,
    options: &TeamMembersGetOptions,
) -> Result<Vec<TeamMember>, StoreError> {
    let rows = sqlx::query_as!(
        TeamMemberRow,
        r#"
        SELECT tm.teamid,
               tm.userid,
               tm.roles,
               tm.deleteat,
               tm.schemeuser,
               tm.schemeadmin,
               tm.schemeguest,
               tm.createat,
               ts.defaultteamguestrole,
               ts.defaultteamuserrole,
               ts.defaultteamadminrole
          FROM teammembers tm
          LEFT JOIN teams t ON tm.teamid = t.id
          LEFT JOIN schemes ts ON t.schemeid = ts.id
          LEFT JOIN users u ON tm.userid = u.id
         WHERE tm.teamid = $1
           AND tm.deleteat = 0
           AND (NOT $4::boolean OR u.deleteat = 0)
         ORDER BY CASE WHEN $5::text = '' THEN tm.userid END,
                  CASE WHEN $5::text = 'Username' THEN u.username END
         LIMIT $3
        OFFSET $2
        "#,
        team_id,
        offset,
        limit,
        options.exclude_deleted_users,
        options.sort
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find TeamMembers with teamId={team_id}"),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());

    Ok(rows.into_iter().map(team_member_from_row).collect())
}

/// Free function so `SqlSessionStore` can reach it without owning a `SqlTeamStore`.
///
/// Go writes this as `me.Team().GetTeamsForUser(...)` — one `SqlStore` exposing every store — and
/// the session store genuinely does depend on the team store. Sharing the pool rather than the
/// struct keeps that dependency without making the two types circular.
#[tracing::instrument(skip(pool), fields(user_id = %user_id))]
pub async fn get_teams_for_user(
    pool: &PgPool,
    user_id: &str,
    exclude_team_id: &str,
    include_deleted: bool,
) -> Result<Vec<TeamMember>, StoreError> {
    // Go assembles this with squirrel and adds the two optional predicates conditionally
    // (team_store.go:1181-1192). Expressing them inside one static statement keeps sqlx's
    // compile-time checking, which is worth more here than mirroring the builder: an empty
    // `exclude_team_id` excludes nothing, and `include_deleted` drops the `DeleteAt` filter.
    //
    // Both joins are LEFT: a team with no scheme — every team on Team Edition, where `Schemes` is
    // an enterprise feature — must still return its member, with the three default role names
    // NULL. An INNER join here would return nothing at all for those members.
    let rows = sqlx::query_as!(
        TeamMemberRow,
        r#"
        SELECT tm.teamid,
               tm.userid,
               tm.roles,
               tm.deleteat,
               tm.schemeuser,
               tm.schemeadmin,
               tm.schemeguest,
               tm.createat,
               ts.defaultteamguestrole,
               ts.defaultteamuserrole,
               ts.defaultteamadminrole
          FROM teammembers tm
          LEFT JOIN teams t ON tm.teamid = t.id
          LEFT JOIN schemes ts ON t.schemeid = ts.id
         WHERE tm.userid = $1
           AND ($2 = '' OR tm.teamid <> $2)
           AND ($3 OR tm.deleteat = 0)
        "#,
        user_id,
        exclude_team_id,
        include_deleted
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find TeamMembers with userId={user_id}"),
        source,
    })?;

    Ok(rows.into_iter().map(team_member_from_row).collect())
}

/// The seven columns both `GetChannelUnreads…` queries select. Named so the two call sites share
/// one decode ([`channel_unread_from_row`]) instead of two copies of the jsonb split — a
/// divergence between them would be invisible on the wire until one route drifted.
///
/// `sqlx::query_as!` binds these by **column name**, not by position, so the order of the fields
/// here and the order of the `SELECT` list are independent. Measured: a mutation reordering two
/// same-typed columns survives a DB test whose expected values for them differ.
struct ChannelUnreadRow {
    teamid: String,
    channelid: String,
    msgcount: i64,
    msgcountroot: i64,
    mentioncount: i64,
    mentioncountroot: i64,
    notifyprops: Option<serde_json::Value>,
}

/// Port of `SqlTeamStore.GetChannelUnreadsForAllTeams` (team_store.go:1231).
///
/// # The exclusion predicate is unconditional, and that is what hides the DMs
///
/// `GetTeamsForUser` in this file adds its `TeamId <> ?` only when an exclusion was asked for.
/// This query does **not**: squirrel renders `sq.NotEq{"TeamId": excludeTeamId}` for the empty
/// string too, as `TeamId <> ''`. Direct and group channels carry an empty `TeamId`, so with no
/// `exclude_team` they are filtered out — and with one, they all pass and surface as a
/// `TeamUnread` whose `team_id` is `""`. Both halves are Go's answer and both are measured by the
/// parity suite; writing the conditional form the sibling uses would leak every DM into the
/// default response.
///
/// # The bare names resolve the same way as in `GetChannelUnread`
///
/// `UserId` is the member's, `DeleteAt` and `TeamId` are the channel's (`ChannelMembers` has
/// neither). So an **archived** channel's unread state is gone here while its membership row
/// survives. `Channels.Type NOT IN ('S')` is the space deny-list (`nonMessageBackingChannelTypes`,
/// channel_store.go:52) — narrower than `GetChannelUnread`'s `IN (O, P, D, G)`: a board's
/// counters **do** feed the team badge here. Unreachable over REST on Team Edition, seeded by
/// the DB test.
///
/// Nothing is coalesced: each selected counter scans into a plain `int64` in Go, so a NULL is a
/// 500, not a zero — the `!` overrides reproduce that. `UrgentMentionCount` is not selected at
/// all and stays at Go's zero value. `NotifyProps` never reaches a client (`json:"-"`) but the
/// app layer branches on it.
#[tracing::instrument(skip(pool), fields(user_id = %user_id, exclude_team_id = %exclude_team_id))]
pub async fn get_channel_unreads_for_all_teams(
    pool: &PgPool,
    exclude_team_id: &str,
    user_id: &str,
) -> Result<Vec<ChannelUnread>, StoreError> {
    let rows = sqlx::query_as!(
        ChannelUnreadRow,
        r#"
        SELECT channels.teamid AS "teamid!",
               channels.id AS "channelid!",
               (channels.totalmsgcount - channelmembers.msgcount) AS "msgcount!",
               (channels.totalmsgcountroot - channelmembers.msgcountroot) AS "msgcountroot!",
               channelmembers.mentioncount AS "mentioncount!",
               channelmembers.mentioncountroot AS "mentioncountroot!",
               channelmembers.notifyprops
          FROM channels
          JOIN channelmembers ON channels.id = channelmembers.channelid
         WHERE channelmembers.userid = $1
           AND channels.deleteat = 0
           AND channels.teamid <> $2
           AND channels.type NOT IN ('S')
        "#,
        user_id,
        exclude_team_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to find Channels with userId={user_id} and teamId!={exclude_team_id}"
        ),
        source,
    })?;
    tracing::Span::current().record("found", rows.len());

    rows.into_iter().map(channel_unread_from_row).collect()
}

/// Port of `SqlTeamStore.GetChannelUnreadsForTeam` (team_store.go:1253).
///
/// The near-twin of [`get_channel_unreads_for_all_teams`], and the difference is the whole
/// reason both exist: this one is `TeamId = ?`, that one `TeamId <> ?`. Same seven selected
/// columns, same `DeleteAt = 0`, same `Channels.Type NOT IN ('S')` space deny-list, same bare
/// names resolving to `ChannelMembers.UserId` and the *channel's* `DeleteAt`/`TeamId`
/// (`ChannelMembers` has neither column) — so an archived channel's unread state is gone here
/// too, and a board's counters still feed the team badge.
///
/// # An equality predicate is not the complement of the inequality one
///
/// The sibling's unconditional `<> ''` is what hides direct and group channels from the default
/// team-list answer, and passing an exclusion surfaces them as a `team_id: ""` entry. Nothing
/// here can produce that entry over REST: `getTeamUnread` reaches this only after
/// `RequireTeamId`, which rejects the empty string, so the `TeamId = ''` query that *would*
/// return every DM is unreachable through the route. The store function itself does not defend
/// against it — Go's does not either.
///
/// Nothing is coalesced (the `!` overrides reproduce Go's plain `int64` scan failing on a NULL),
/// `UrgentMentionCount` is not selected and stays at zero, and `NotifyProps` never reaches a
/// client but the app-layer fold branches on it.
#[tracing::instrument(skip(pool), fields(user_id = %user_id, team_id = %team_id))]
pub async fn get_channel_unreads_for_team(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
) -> Result<Vec<ChannelUnread>, StoreError> {
    let rows = sqlx::query_as!(
        ChannelUnreadRow,
        r#"
        SELECT channels.teamid AS "teamid!",
               channels.id AS "channelid!",
               (channels.totalmsgcount - channelmembers.msgcount) AS "msgcount!",
               (channels.totalmsgcountroot - channelmembers.msgcountroot) AS "msgcountroot!",
               channelmembers.mentioncount AS "mentioncount!",
               channelmembers.mentioncountroot AS "mentioncountroot!",
               channelmembers.notifyprops
          FROM channels
          JOIN channelmembers ON channels.id = channelmembers.channelid
         WHERE channelmembers.userid = $1
           AND channels.teamid = $2
           AND channels.deleteat = 0
           AND channels.type NOT IN ('S')
        "#,
        user_id,
        team_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find Channels with teamId={team_id} and userId={user_id}"),
        source,
    })?;
    tracing::Span::current().record("found", rows.len());

    rows.into_iter().map(channel_unread_from_row).collect()
}

/// The shared row decode of the two `GetChannelUnreads…` queries: they select the same seven
/// columns in the same order, so the mapping is written once.
fn channel_unread_from_row(row: ChannelUnreadRow) -> Result<ChannelUnread, StoreError> {
    // Same jsonb split as `channel_store::get_channel_unread`: SQL NULL and the JSON
    // value `null` are different rows, and both are a nil map in Go ([D-135]).
    let notify_props = match row.notifyprops {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value::<StringMap>(value).map_err(|source| StoreError::Decode {
                entity: "ChannelUnread",
                column: "notifyprops",
                source,
            })?,
        ),
    };
    Ok(ChannelUnread {
        team_id: row.teamid,
        channel_id: row.channelid,
        msg_count: row.msgcount,
        mention_count: row.mentioncount,
        mention_count_root: row.mentioncountroot,
        urgent_mention_count: 0,
        msg_count_root: row.msgcountroot,
        notify_props,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact row in the development database, and the exact answer the running Go server
    /// gives for it (`GET /api/v4/users/me/teams/members`):
    ///
    /// ```json
    /// {"roles":"team_user team_admin","scheme_user":true,"scheme_admin":true,
    ///  "scheme_guest":false,"explicit_roles":""}
    /// ```
    ///
    /// Measured, not predicted — this is the one case with a live oracle behind it.
    #[test]
    fn matches_the_running_go_server_for_a_scheme_less_team() {
        let info = get_team_roles(false, true, true, "", "", "", "");
        assert_eq!(info.roles.join(" "), "team_user team_admin");
        assert_eq!(info.explicit_roles.join(" "), "");
        assert!(!info.scheme_guest);
        assert!(info.scheme_user);
        assert!(info.scheme_admin);
    }

    /// The un-migrated case Go's comment describes: a scheme role sitting in the `Roles` column
    /// sets the flag even though the column said false, and is kept out of `explicit_roles`.
    #[test]
    fn a_scheme_role_in_the_roles_column_sets_the_flag_and_is_not_explicit() {
        let info = get_team_roles(false, false, false, "", "", "", "team_admin");
        assert!(
            info.scheme_admin,
            "the column said false; the role says true"
        );
        assert_eq!(info.explicit_roles, Vec::<String>::new());
        assert_eq!(info.roles.join(" "), "team_admin");
    }

    /// A role that is not one of the three is explicit, appears in both outputs, and keeps its
    /// position ahead of the implied roles.
    #[test]
    fn custom_roles_are_explicit_and_come_first() {
        let info = get_team_roles(false, true, false, "", "", "", "custom_one custom_two");
        assert_eq!(info.explicit_roles, vec!["custom_one", "custom_two"]);
        assert_eq!(info.roles.join(" "), "custom_one custom_two team_user");
    }

    /// With a scheme, the implied role is the scheme's name rather than the constant.
    #[test]
    fn scheme_defaults_replace_the_constants() {
        let info = get_team_roles(
            true,
            true,
            true,
            "custom_guest",
            "custom_user",
            "custom_admin",
            "",
        );
        assert_eq!(
            info.roles.join(" "),
            "custom_guest custom_user custom_admin"
        );
    }

    /// Only the flags that are set contribute, and the order is always guest, user, admin —
    /// never the order the flags were discovered in.
    #[test]
    fn implied_roles_are_emitted_in_guest_user_admin_order() {
        let info = get_team_roles(true, false, true, "", "", "", "");
        assert_eq!(info.roles.join(" "), "team_guest team_admin");
    }

    /// The dedup check reads `roles` as it grows, so two scheme defaults with the same name
    /// collapse. Reproduced because it is Go's behaviour, not because it is desirable.
    #[test]
    fn identical_scheme_defaults_collapse_to_one_role() {
        let info = get_team_roles(false, true, true, "", "same_role", "same_role", "");
        assert_eq!(info.roles.join(" "), "same_role");
        assert!(info.scheme_user && info.scheme_admin, "both flags stay set");
    }

    /// An implied role already present as an explicit role is not appended twice.
    #[test]
    fn an_implied_role_already_explicit_is_not_duplicated() {
        let info = get_team_roles(false, true, false, "", "custom_user", "", "custom_user");
        assert_eq!(info.roles.join(" "), "custom_user");
        assert_eq!(info.explicit_roles, vec!["custom_user"]);
    }

    /// `strings.Fields` drops empty fields, so runs of whitespace and a blank column both yield
    /// nothing. A NULL column reaches this as `""` via `unwrap_or_default`.
    #[test]
    fn whitespace_only_roles_contribute_nothing() {
        for input in ["", "   ", "\t\n ", "  \t"] {
            let info = get_team_roles(false, false, false, "", "", "", input);
            assert!(
                info.roles.is_empty() && info.explicit_roles.is_empty(),
                "input {input:?} should contribute no roles"
            );
        }

        // And the separators between real roles are runs, not single spaces.
        let info = get_team_roles(false, false, false, "", "", "", "  a \t\n b  ");
        assert_eq!(info.explicit_roles, vec!["a", "b"]);
    }

    /// No flags and no roles is an empty result, not a defaulted one — a member of a team with
    /// no roles at all serialises `"roles": ""`.
    #[test]
    fn nothing_set_yields_nothing() {
        let info = get_team_roles(false, false, false, "", "", "", "");
        assert_eq!(info, RolesInfo::default());
        assert_eq!(info.roles.join(" "), "");
    }
}
