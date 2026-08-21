//! Port of `SqlChannelStore` (channels/store/sqlstore/channel_store.go), `GetMember` only,
//! together with the channel scheme-roles machinery it exists to drive.
//!
//! # Why this file exists now
//!
//! [D-134] lists what is missing from `app/authorization.go`, and the largest and most valuable
//! group — `SessionHasPermissionToChannel` and the six checks behind it — is blocked on exactly
//! one thing: a way to load a `ChannelMember` with its **effective** roles. That is `GetMember`,
//! and the roles it returns are computed rather than stored.
//!
//! # Three levels of fallback, not two
//!
//! [`crate::team_store::get_team_roles`] resolves a team member's roles from the team's scheme or
//! a constant. A channel member has one more level: the **channel's** scheme wins, the **team's**
//! scheme is the fallback, and the constant is the last resort. Getting that order wrong is a
//! silent permission difference — a channel admin quietly holding the team scheme's role instead
//! of the channel scheme's, with a different permission set behind it.
//!
//! # The column names in the SELECT are a trap
//!
//! The team-scheme fallback reads `TeamScheme.DefaultChannel*Role` — the team scheme's
//! **channel** role defaults, not its team ones (channel_store.go:569-571). `DefaultTeamUserRole`
//! is the right-looking column and the wrong one: it is what a *team member* falls back to, and
//! substituting it here would hand channel members a team-scoped role name that
//! `RolesGrantPermission` would then resolve against a completely different permission set.
//! The parameter names below keep Go's (`default_team_user_role`), because Go's `getChannelRoles`
//! signature does; the doc comment is where the distinction lives.

use std::collections::HashMap;

use mm_model::channel::{Channel, ChannelBannerInfo, ChannelSearchOpts};
use mm_model::channel_list::ChannelList;
use mm_model::channel_member::{ChannelMember, ChannelUnread};
use mm_model::role::{CHANNEL_ADMIN_ROLE_ID, CHANNEL_GUEST_ROLE_ID, CHANNEL_USER_ROLE_ID};
use mm_model::utils::StringMap;
use sqlx::PgPool;

use crate::error::StoreError;
use crate::team_store::RolesInfo;

/// Port of `getChannelRoles` (channel_store.go:248).
///
/// Two passes, and the order of both is on the wire because the result is joined with spaces.
///
/// 1. **Split `Roles`.** A role matching one of the three channel scheme role ids sets the
///    corresponding scheme flag — *even when the column said false* — and is dropped from both
///    outputs. This is the un-migrated case Go's comment describes. Anything else lands in
///    `explicit_roles` **and** `roles`, in the order it appeared.
/// 2. **Append the implied roles**, guest then user then admin. Each is resolved by a
///    three-level fallback: the **channel** scheme's default, else the **team** scheme's default,
///    else the constant. Each is skipped if it is already present in `roles`.
///
/// `default_team_*_role` here means "the team scheme's default *channel* role" — see the module
/// docs. The dedup check reads `result.roles` as it grows, so two defaults that happen to be the
/// same string collapse to one; that is Go's behaviour, not an accident of this port.
#[allow(clippy::too_many_arguments)] // Go's signature; splitting it would obscure the porting map.
pub fn get_channel_roles(
    scheme_guest: bool,
    scheme_user: bool,
    scheme_admin: bool,
    default_team_guest_role: &str,
    default_team_user_role: &str,
    default_team_admin_role: &str,
    default_channel_guest_role: &str,
    default_channel_user_role: &str,
    default_channel_admin_role: &str,
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
    // `split_whitespace` consults the same Unicode White_Space property, so an empty or all-blank
    // column yields no roles on both sides.
    for role in roles.split_whitespace() {
        match role {
            CHANNEL_GUEST_ROLE_ID => result.scheme_guest = true,
            CHANNEL_USER_ROLE_ID => result.scheme_user = true,
            CHANNEL_ADMIN_ROLE_ID => result.scheme_admin = true,
            other => {
                result.explicit_roles.push(other.to_owned());
                result.roles.push(other.to_owned());
            }
        }
    }

    /// Channel scheme first, team scheme second, constant last (channel_store.go:277-303).
    fn implied<'a>(channel_default: &'a str, team_default: &'a str, constant: &'a str) -> &'a str {
        if !channel_default.is_empty() {
            channel_default
        } else if !team_default.is_empty() {
            team_default
        } else {
            constant
        }
    }

    // Scheme-implied roles, in Go's order: guest, user, admin.
    let mut implied_roles: Vec<&str> = Vec::new();
    if result.scheme_guest {
        implied_roles.push(implied(
            default_channel_guest_role,
            default_team_guest_role,
            CHANNEL_GUEST_ROLE_ID,
        ));
    }
    if result.scheme_user {
        implied_roles.push(implied(
            default_channel_user_role,
            default_team_user_role,
            CHANNEL_USER_ROLE_ID,
        ));
    }
    if result.scheme_admin {
        implied_roles.push(implied(
            default_channel_admin_role,
            default_team_admin_role,
            CHANNEL_ADMIN_ROLE_ID,
        ));
    }

    for implied_role in implied_roles {
        if !result.roles.iter().any(|role| role == implied_role) {
            result.roles.push(implied_role.to_owned());
        }
    }

    result
}

/// The subset of Go's `store.ChannelStore` (store/store.go:200-386) that is ported.
pub trait ChannelStore {
    /// Port of `SqlChannelStore.Get` (channel_store.go:985).
    fn get(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<Channel, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetMember` (channel_store.go:2440).
    fn get_member(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<ChannelMember, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetAllChannelMembersForUser` (channel_store.go:2527).
    fn get_all_channel_members_for_user(
        &self,
        user_id: &str,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<HashMap<String, String>, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetChannelUnread` (channel_store.go:921).
    fn get_channel_unread(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<ChannelUnread, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetByNames` (channel_store.go:1634).
    ///
    /// Go's third parameter, `allowFromCache`, is dropped: this port has no channel-by-name
    /// cache, so every call behaves as `false` — never staler than Go, same rows.
    fn get_by_names(
        &self,
        team_id: &str,
        names: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Channel>, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetByName` / `GetByNameIncludeDeleted` (channel_store.go:1676,
    /// :1680) — Go's two one-line wrappers over `getByName`, folded into the flag they differ by.
    /// `allowFromCache` dropped as in [`ChannelStore::get_by_names`].
    fn get_by_name(
        &self,
        team_id: &str,
        name: &str,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<Channel, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetChannels` (channel_store.go:1208): the channels of one user
    /// in one team, display-name order, `ErrNotFound` when there are none.
    fn get_channels(
        &self,
        team_id: &str,
        user_id: &str,
        opts: &ChannelSearchOpts,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetMemberCount` (channel_store.go:2666).
    ///
    /// Go's `allowFromCache` is dropped like `get_by_names`'s: no cache, never staler than Go.
    fn get_member_count(
        &self,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetGuestCount` (channel_store.go:2752). `allowFromCache` dropped.
    fn get_guest_count(
        &self,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetPinnedPostCount` (channel_store.go:2731). `allowFromCache`
    /// dropped.
    fn get_pinned_post_count(
        &self,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetFileCount` (channel_store.go:2646).
    fn get_file_count(
        &self,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetMembers` (channel_store.go:2181). Go's
    /// `ChannelMembersGetOptions` is flattened to the three fields ported callers use.
    fn get_members(
        &self,
        channel_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<ChannelMember>, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetMembersForUser` (channel_store.go:3261): every membership
    /// of one user in one team's channels, plus the teamless ones.
    fn get_members_for_user(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ChannelMember>, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlChannelStore {
    pool: PgPool,
}

impl SqlChannelStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ChannelStore for SqlChannelStore {
    #[tracing::instrument(skip_all, fields(channel_id = %id, found))]
    async fn get(&self, id: &str) -> Result<Channel, StoreError> {
        get(&self.pool, id).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id, found))]
    async fn get_member(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<ChannelMember, StoreError> {
        get_member(&self.pool, channel_id, user_id).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, channels))]
    async fn get_all_channel_members_for_user(
        &self,
        user_id: &str,
        include_deleted: bool,
    ) -> Result<HashMap<String, String>, StoreError> {
        get_all_channel_members_for_user(&self.pool, user_id, include_deleted).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id, found))]
    async fn get_channel_unread(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<ChannelUnread, StoreError> {
        get_channel_unread(&self.pool, channel_id, user_id).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, names = names.len(), found))]
    async fn get_by_names(
        &self,
        team_id: &str,
        names: &[String],
    ) -> Result<Vec<Channel>, StoreError> {
        get_by_names(&self.pool, team_id, names).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, name = %name, include_deleted, found))]
    async fn get_by_name(
        &self,
        team_id: &str,
        name: &str,
        include_deleted: bool,
    ) -> Result<Channel, StoreError> {
        get_by_name(&self.pool, team_id, name, include_deleted).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, count))]
    async fn get_channels(
        &self,
        team_id: &str,
        user_id: &str,
        opts: &ChannelSearchOpts,
    ) -> Result<ChannelList, StoreError> {
        get_channels(&self.pool, team_id, user_id, opts).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    async fn get_member_count(&self, channel_id: &str) -> Result<i64, StoreError> {
        get_member_count(&self.pool, channel_id).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    async fn get_guest_count(&self, channel_id: &str) -> Result<i64, StoreError> {
        get_guest_count(&self.pool, channel_id).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    async fn get_pinned_post_count(&self, channel_id: &str) -> Result<i64, StoreError> {
        get_pinned_post_count(&self.pool, channel_id).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    async fn get_file_count(&self, channel_id: &str) -> Result<i64, StoreError> {
        get_file_count(&self.pool, channel_id).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    async fn get_members(
        &self,
        channel_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<ChannelMember>, StoreError> {
        get_members(&self.pool, channel_id, offset, limit).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id))]
    async fn get_members_for_user(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> Result<Vec<ChannelMember>, StoreError> {
        get_members_for_user(&self.pool, team_id, user_id).await
    }
}

/// One row of Go's `channelMembersForTeamWithSchemeSelectQuery` (channel_store.go:558) — the
/// membership columns plus the two schemes' channel-role defaults. Both `GetMember` and
/// `GetMembers` select exactly this shape, so the row-to-model mapping lives once in
/// [`channel_member_from_row`].
struct ChannelMemberRow {
    channelid: String,
    userid: String,
    roles: Option<String>,
    lastviewedat: Option<i64>,
    msgcount: Option<i64>,
    mentioncount: Option<i64>,
    mentioncountroot: Option<i64>,
    urgentmentioncount: i64,
    msgcountroot: Option<i64>,
    notifyprops: Option<serde_json::Value>,
    lastupdateat: Option<i64>,
    schemeuser: Option<bool>,
    schemeadmin: Option<bool>,
    schemeguest: Option<bool>,
    teamschemedefaultguestrole: Option<String>,
    teamschemedefaultuserrole: Option<String>,
    teamschemedefaultadminrole: Option<String>,
    channelschemedefaultguestrole: Option<String>,
    channelschemedefaultuserrole: Option<String>,
    channelschemedefaultadminrole: Option<String>,
    autotranslationdisabled: bool,
}

/// Port of `channelMemberWithSchemeRoles.ToModel` (channel_store.go:313), shared by both member
/// lookups.
///
/// `notifyprops` is `jsonb`, so SQL NULL and the JSON value `null` are different rows and the
/// Go server writes both. Go's `json.Unmarshal` turns a JSON null into a nil map without
/// complaint, so only a *type* mismatch is an error — see [D-135], where treating JSON null as
/// a decode failure made `GET /users/me` a 500 for four users out of five.
///
/// Go's `sql.NullBool` and `sql.NullString` both mean "NULL is the zero value" here —
/// `Valid && Bool` for the flags, `""` for the role names — so `unwrap_or_default` is the same
/// rule, not a looser one.
fn channel_member_from_row(row: ChannelMemberRow) -> Result<ChannelMember, StoreError> {
    let notify_props = match row.notifyprops {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value::<StringMap>(value).map_err(|source| StoreError::Decode {
                entity: "ChannelMember",
                column: "notifyprops",
                source,
            })?,
        ),
    };

    let roles_result = get_channel_roles(
        row.schemeguest.unwrap_or_default(),
        row.schemeuser.unwrap_or_default(),
        row.schemeadmin.unwrap_or_default(),
        row.teamschemedefaultguestrole
            .as_deref()
            .unwrap_or_default(),
        row.teamschemedefaultuserrole.as_deref().unwrap_or_default(),
        row.teamschemedefaultadminrole
            .as_deref()
            .unwrap_or_default(),
        row.channelschemedefaultguestrole
            .as_deref()
            .unwrap_or_default(),
        row.channelschemedefaultuserrole
            .as_deref()
            .unwrap_or_default(),
        row.channelschemedefaultadminrole
            .as_deref()
            .unwrap_or_default(),
        row.roles.as_deref().unwrap_or_default(),
    );

    Ok(ChannelMember {
        channel_id: row.channelid,
        user_id: row.userid,
        roles: roles_result.roles.join(" "),
        last_viewed_at: row.lastviewedat.unwrap_or_default(),
        msg_count: row.msgcount.unwrap_or_default(),
        msg_count_root: row.msgcountroot.unwrap_or_default(),
        mention_count: row.mentioncount.unwrap_or_default(),
        mention_count_root: row.mentioncountroot.unwrap_or_default(),
        urgent_mention_count: row.urgentmentioncount,
        notify_props,
        last_update_at: row.lastupdateat.unwrap_or_default(),
        scheme_admin: roles_result.scheme_admin,
        scheme_user: roles_result.scheme_user,
        scheme_guest: roles_result.scheme_guest,
        explicit_roles: roles_result.explicit_roles.join(" "),
        auto_translation_disabled: row.autotranslationdisabled,
    })
}

/// Free function so the app layer's permission checks can reach it without owning a
/// `SqlChannelStore`, mirroring [`crate::team_store::get_teams_for_user`].
///
/// Go's signature takes an `rctx request.CTX` and uses it to pick the **master or a replica**
/// handle (context.go:31). This port has one pool and always reads the master — strictly the
/// safer direction, never staler than Go. See [D-140].
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id, user_id = %user_id))]
pub async fn get_member(
    pool: &PgPool,
    channel_id: &str,
    user_id: &str,
) -> Result<ChannelMember, StoreError> {
    // Go's `channelMembersForTeamWithSchemeSelectQuery` (channel_store.go:558) with the two
    // equality predicates `GetMember` adds. The join shape is Go's exactly:
    //
    //   - **INNER** on `Channels`. A membership row whose channel is gone returns *nothing*, not
    //     a member with empty scheme defaults. Widening this to a LEFT join would resurrect
    //     orphaned memberships, and a permission check reading one would grant against a channel
    //     that no longer exists.
    //   - **LEFT** on the two `Schemes` rows and on `Teams`. Every channel on Team Edition has a
    //     NULL `SchemeId` — `Schemes` is an enterprise feature and the table is empty — so an
    //     INNER join anywhere in that chain would return no members at all. `Teams` is LEFT
    //     because a DM or GM channel has an empty `TeamId` and matches no team.
    //
    // `COALESCE(UrgentMentionCount, 0)` is Go's, reproduced in SQL rather than defaulted
    // Rust-side so the database answers the same question for both servers.
    let row = sqlx::query_as!(
        ChannelMemberRow,
        r#"
        SELECT cm.channelid,
               cm.userid,
               cm.roles,
               cm.lastviewedat,
               cm.msgcount,
               cm.mentioncount,
               cm.mentioncountroot,
               COALESCE(cm.urgentmentioncount, 0) AS "urgentmentioncount!",
               cm.msgcountroot,
               cm.notifyprops,
               cm.lastupdateat,
               cm.schemeuser,
               cm.schemeadmin,
               cm.schemeguest,
               teamscheme.defaultchannelguestrole    AS teamschemedefaultguestrole,
               teamscheme.defaultchanneluserrole     AS teamschemedefaultuserrole,
               teamscheme.defaultchanneladminrole    AS teamschemedefaultadminrole,
               channelscheme.defaultchannelguestrole AS channelschemedefaultguestrole,
               channelscheme.defaultchanneluserrole  AS channelschemedefaultuserrole,
               channelscheme.defaultchanneladminrole AS channelschemedefaultadminrole,
               cm.autotranslationdisabled
          FROM channelmembers cm
          INNER JOIN channels c ON cm.channelid = c.id
          LEFT JOIN schemes channelscheme ON c.schemeid = channelscheme.id
          LEFT JOIN teams t ON c.teamid = t.id
          LEFT JOIN schemes teamscheme ON t.schemeid = teamscheme.id
         WHERE cm.channelid = $1
           AND cm.userid = $2
        "#,
        channel_id,
        user_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to get ChannelMember with channelId={channel_id} and userId={user_id}"
        ),
        source,
    })?;

    let Some(row) = row else {
        tracing::Span::current().record("found", false);
        return Err(StoreError::NotFound {
            entity: "ChannelMember",
            criteria: format!("channelId={channel_id}, userId={user_id}"),
        });
    };
    tracing::Span::current().record("found", true);

    channel_member_from_row(row)
}

/// Port of `SqlChannelStore.GetMembers` (channel_store.go:2181), the paginated member list.
///
/// Three of Go's decisions ride along and each is a trap:
///
/// - **`Limit > 0` and `Offset > 0` are guards, not clamps** — squirrel adds the clause only
///   when positive, so `limit = 0` means *no limit* (the whole channel), not zero rows. The
///   api4 route can produce exactly that: `?per_page=0` passes the parser (`0` is not negative)
///   and Go serves every member. Expressed here as `CASE WHEN`, since Postgres treats
///   `LIMIT NULL`/`OFFSET NULL` as absent.
/// - **No `ORDER BY`.** Pagination over heap order — Go adds an ordering only in the
///   `UpdatedAfter` variant, which no ported route uses. Both servers run the same query
///   against the same table, so they page identically; that is a property of the shared
///   database, not a wire guarantee.
/// - `opts.UpdatedAfter` is dropped with the rest of `ChannelMembersGetOptions` — no ported
///   caller sets it, and a parameter no caller can use is a lie at the call site (the
///   `allowFromCache` rule).
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id, offset, limit, found))]
pub async fn get_members(
    pool: &PgPool,
    channel_id: &str,
    offset: i64,
    limit: i64,
) -> Result<Vec<ChannelMember>, StoreError> {
    let rows = sqlx::query_as!(
        ChannelMemberRow,
        r#"
        SELECT cm.channelid,
               cm.userid,
               cm.roles,
               cm.lastviewedat,
               cm.msgcount,
               cm.mentioncount,
               cm.mentioncountroot,
               COALESCE(cm.urgentmentioncount, 0) AS "urgentmentioncount!",
               cm.msgcountroot,
               cm.notifyprops,
               cm.lastupdateat,
               cm.schemeuser,
               cm.schemeadmin,
               cm.schemeguest,
               teamscheme.defaultchannelguestrole    AS teamschemedefaultguestrole,
               teamscheme.defaultchanneluserrole     AS teamschemedefaultuserrole,
               teamscheme.defaultchanneladminrole    AS teamschemedefaultadminrole,
               channelscheme.defaultchannelguestrole AS channelschemedefaultguestrole,
               channelscheme.defaultchanneluserrole  AS channelschemedefaultuserrole,
               channelscheme.defaultchanneladminrole AS channelschemedefaultadminrole,
               cm.autotranslationdisabled
          FROM channelmembers cm
          INNER JOIN channels c ON cm.channelid = c.id
          LEFT JOIN schemes channelscheme ON c.schemeid = channelscheme.id
          LEFT JOIN teams t ON c.teamid = t.id
          LEFT JOIN schemes teamscheme ON t.schemeid = teamscheme.id
         WHERE cm.channelid = $1
         LIMIT CASE WHEN $2::bigint > 0 THEN $2::bigint END
        OFFSET CASE WHEN $3::bigint > 0 THEN $3::bigint END
        "#,
        channel_id,
        limit,
        offset
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get ChannelMembers with channelId={channel_id}"),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());

    rows.into_iter().map(channel_member_from_row).collect()
}

/// Port of `SqlChannelStore.GetMembersForUser` (channel_store.go:3261): the caller's memberships
/// across a team, the body of `GET /users/{user_id}/teams/{team_id}/channels/members`.
///
/// The same `channelMembersForTeamWithSchemeSelectQuery` as [`get_member`] with three
/// predicates, each of which a reader would plausibly write differently:
///
/// - **The team predicate is on `Teams.Id`, through the LEFT join — not on `Channels.TeamId`.**
///   Go writes `Teams.Id = ? OR Teams.Id = '' OR Teams.Id IS NULL`; a DM or GM has an empty
///   `Channels.TeamId`, matches no team row, and arrives as NULL — so every teamless membership
///   is in every team's answer, exactly as [`get_channels`] lists every DM under every team. The
///   `= ''` arm can never match (no team has an empty id) and is kept only because dropping it
///   is a mutation a test cannot see. A membership whose channel names a team that no longer
///   exists *also* arrives as NULL and is listed under every team — reproduced, not repaired.
/// - **No `DeleteAt` filter at all** — neither the channel's nor anything else's. An archived
///   channel's membership is in the list (the sibling channel list hides the channel by default).
///   The DB test pins it.
/// - **`Channels.Type NOT IN ('S')`** (`nonMessageBackingChannelTypes`, channel_store.go:52):
///   a Space's backing channel is excluded; boards are not, unlike [`get_channels`]'s allow-list.
///
/// No `ORDER BY` — heap order, shared with Go through the shared table and nothing else.
#[tracing::instrument(skip(pool), fields(team_id = %team_id, user_id = %user_id, found))]
pub async fn get_members_for_user(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
) -> Result<Vec<ChannelMember>, StoreError> {
    let rows = sqlx::query_as!(
        ChannelMemberRow,
        r#"
        SELECT cm.channelid,
               cm.userid,
               cm.roles,
               cm.lastviewedat,
               cm.msgcount,
               cm.mentioncount,
               cm.mentioncountroot,
               COALESCE(cm.urgentmentioncount, 0) AS "urgentmentioncount!",
               cm.msgcountroot,
               cm.notifyprops,
               cm.lastupdateat,
               cm.schemeuser,
               cm.schemeadmin,
               cm.schemeguest,
               teamscheme.defaultchannelguestrole    AS teamschemedefaultguestrole,
               teamscheme.defaultchanneluserrole     AS teamschemedefaultuserrole,
               teamscheme.defaultchanneladminrole    AS teamschemedefaultadminrole,
               channelscheme.defaultchannelguestrole AS channelschemedefaultguestrole,
               channelscheme.defaultchanneluserrole  AS channelschemedefaultuserrole,
               channelscheme.defaultchanneladminrole AS channelschemedefaultadminrole,
               cm.autotranslationdisabled
          FROM channelmembers cm
          INNER JOIN channels c ON cm.channelid = c.id
          LEFT JOIN schemes channelscheme ON c.schemeid = channelscheme.id
          LEFT JOIN teams t ON c.teamid = t.id
          LEFT JOIN schemes teamscheme ON t.schemeid = teamscheme.id
         WHERE cm.userid = $1
           AND (t.id = $2 OR t.id = '' OR t.id IS NULL)
           AND c.type NOT IN ('S')
        "#,
        user_id,
        team_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to find ChannelMembers data with teamId={team_id} and userId={user_id}"
        ),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());

    rows.into_iter().map(channel_member_from_row).collect()
}

/// The row shape of Go's `channelSliceColumns(true)` (channel_store.go:159): every `Channels`
/// column the store selects, plus the two access-control flags computed per row. Shared by
/// [`get`] and [`get_by_names`] so the two queries cannot drift in what they select or how a row
/// becomes a [`Channel`].
struct ChannelRow {
    id: String,
    createat: Option<i64>,
    updateat: Option<i64>,
    deleteat: Option<i64>,
    teamid: Option<String>,
    channel_type: String,
    displayname: Option<String>,
    name: Option<String>,
    header: Option<String>,
    purpose: Option<String>,
    lastpostat: Option<i64>,
    totalmsgcount: Option<i64>,
    extraupdateat: Option<i64>,
    creatorid: Option<String>,
    schemeid: Option<String>,
    groupconstrained: Option<bool>,
    autotranslation: bool,
    shared: Option<bool>,
    totalmsgcountroot: Option<i64>,
    lastrootpostat: Option<i64>,
    bannerinfo: Option<serde_json::Value>,
    defaultcategoryname: String,
    discoverable: bool,
    policy_enforced: bool,
    policy_is_active: bool,
}

/// Port of `channelSliceColumns`'s scan target becoming a `model.Channel`.
fn channel_from_row(row: ChannelRow) -> Result<Channel, StoreError> {
    // `bannerinfo` is `jsonb`, so the same SQL-NULL-versus-JSON-`null` split as [D-135] applies.
    let banner_info = match row.bannerinfo {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(serde_json::from_value::<ChannelBannerInfo>(value).map_err(
            |source| StoreError::Decode {
                entity: "Channel",
                column: "bannerinfo",
                source,
            },
        )?),
    };

    Ok(Channel {
        id: row.id,
        create_at: row.createat.unwrap_or_default(),
        update_at: row.updateat.unwrap_or_default(),
        delete_at: row.deleteat.unwrap_or_default(),
        team_id: row.teamid.unwrap_or_default(),
        channel_type: row.channel_type,
        display_name: row.displayname.unwrap_or_default(),
        name: row.name.unwrap_or_default(),
        header: row.header.unwrap_or_default(),
        purpose: row.purpose.unwrap_or_default(),
        last_post_at: row.lastpostat.unwrap_or_default(),
        total_msg_count: row.totalmsgcount.unwrap_or_default(),
        extra_update_at: row.extraupdateat.unwrap_or_default(),
        creator_id: row.creatorid.unwrap_or_default(),
        scheme_id: row.schemeid,
        group_constrained: row.groupconstrained,
        auto_translation: row.autotranslation,
        shared: row.shared,
        total_msg_count_root: row.totalmsgcountroot.unwrap_or_default(),
        last_root_post_at: row.lastrootpostat.unwrap_or_default(),
        banner_info,
        default_category_name: row.defaultcategoryname,
        discoverable: row.discoverable,
        policy_enforced: row.policy_enforced,
        policy_is_active: row.policy_is_active,

        // Not selected by Go's `channelSliceColumns`; each is filled elsewhere or left zero.
        //   props                  — written by the store, never read back by these queries
        //   policy_id              — set by the access-control layer
        //   policy_actions         — hydrated by `App.HydrateChannelPolicyActions`, see [D-141]
        //   managed_category_name  — set by the sidebar layer
        props: None,
        policy_id: None,
        policy_actions: None,
        managed_category_name: String::new(),
    })
}

/// Port of `SqlChannelStore.Get` (channel_store.go:985).
///
/// Two things in this query are easy to drop and both change what the caller sees:
///
/// - **`Type IN (O, P, D, G)`.** `Get` is not "the channel with this id" — it is "the *message*
///   channel with this id" (`messageChannelTypes`, channel_store.go:39). A board (`BO`/`BP`) or a
///   space (`S`) has a `Channels` row and is deliberately invisible here; Go reaches those through
///   `GetBoardChannel` and `GetChannelOfType` instead. Widening this to a bare id lookup makes a
///   permission check answer questions about a channel Go would have called missing.
/// - **The two `AccessControlPolicies` subqueries.** `PolicyEnforced` and `PolicyIsActive` are not
///   columns; they are computed per row by `channelSliceColumns(true)` (channel_store.go:186-188).
///   Defaulting them to `false` Rust-side would silently claim no channel is policy-enforced.
///
/// `Props`, `PolicyId`, `ManagedCategoryName` and `PolicyActions` are **not** selected by Go
/// either — they are hydrated by other call sites — so they stay at their zero values here rather
/// than being invented.
#[tracing::instrument(skip(pool), fields(channel_id = %id))]
pub async fn get(pool: &PgPool, id: &str) -> Result<Channel, StoreError> {
    let row = sqlx::query_as!(
        ChannelRow,
        r#"
        SELECT c.id,
               c.createat,
               c.updateat,
               c.deleteat,
               c.teamid,
               c.type::text AS "channel_type!",
               c.displayname,
               c.name,
               c.header,
               c.purpose,
               c.lastpostat,
               c.totalmsgcount,
               c.extraupdateat,
               c.creatorid,
               c.schemeid,
               c.groupconstrained,
               c.autotranslation,
               c.shared,
               c.totalmsgcountroot,
               c.lastrootpostat,
               c.bannerinfo,
               c.defaultcategoryname,
               c.discoverable,
               EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp
                    WHERE acp.id = c.id AND acp.type = 'channel'
               ) AS "policy_enforced!",
               COALESCE((
                   SELECT acp.active FROM accesscontrolpolicies acp
                    WHERE acp.id = c.id AND acp.type = 'channel' AND acp.active = TRUE
                    LIMIT 1
               ), false) AS "policy_is_active!"
          FROM channels c
         WHERE c.id = $1
           AND c.type IN ('O', 'P', 'D', 'G')
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find channel with id = {id}"),
        source,
    })?;

    let Some(row) = row else {
        tracing::Span::current().record("found", false);
        return Err(StoreError::NotFound {
            entity: "Channel",
            criteria: id.to_owned(),
        });
    };
    tracing::Span::current().record("found", true);

    channel_from_row(row)
}

/// Port of `SqlChannelStore.getByNames` (channel_store.go:1638) as its exported non-archived
/// variant, `GetByNames` (:1634).
///
/// Three predicates and a guard, all Go's:
///
/// - **`len(names) > 0` short-circuits before any SQL.** An empty list returns an empty slice
///   without touching the database — reproduced here, so mentioning nothing costs nothing.
/// - **`Type IN (O, P, D, G)`** — `messageChannelTypes` again, same reasoning as [`get`].
/// - **`DeleteAt = 0`**: the non-archived variant. `FillInChannelProps` links only living
///   channels, so a `~mention` of an archived channel renders as plain text.
/// - **The team filter only exists when `teamId` is non-empty** (channel_store.go:1656). Go
///   *omits the predicate* rather than comparing against `''`, so an empty team id searches every
///   team — that is what a DM/GM channel (whose `TeamId` is `""`) passes down. The `$2 = ''` OR
///   below is the same rule in one statement instead of two.
///
/// Go applies no `ORDER BY`; every caller builds a name-keyed map. The row order here is
/// whatever Postgres returns, and nothing downstream may depend on it.
#[tracing::instrument(skip(pool, names), fields(team_id = %team_id, names = names.len()))]
pub async fn get_by_names(
    pool: &PgPool,
    team_id: &str,
    names: &[String],
) -> Result<Vec<Channel>, StoreError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query_as!(
        ChannelRow,
        r#"
        SELECT c.id,
               c.createat,
               c.updateat,
               c.deleteat,
               c.teamid,
               c.type::text AS "channel_type!",
               c.displayname,
               c.name,
               c.header,
               c.purpose,
               c.lastpostat,
               c.totalmsgcount,
               c.extraupdateat,
               c.creatorid,
               c.schemeid,
               c.groupconstrained,
               c.autotranslation,
               c.shared,
               c.totalmsgcountroot,
               c.lastrootpostat,
               c.bannerinfo,
               c.defaultcategoryname,
               c.discoverable,
               EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp
                    WHERE acp.id = c.id AND acp.type = 'channel'
               ) AS "policy_enforced!",
               COALESCE((
                   SELECT acp.active FROM accesscontrolpolicies acp
                    WHERE acp.id = c.id AND acp.type = 'channel' AND acp.active = TRUE
                    LIMIT 1
               ), false) AS "policy_is_active!"
          FROM channels c
         WHERE c.name = ANY($1)
           AND c.type IN ('O', 'P', 'D', 'G')
           AND c.deleteat = 0
           AND ($2::text = '' OR c.teamid = $2)
        "#,
        names,
        team_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get channels with names={names:?} teamId={team_id}"),
        source,
    })?;

    rows.into_iter().map(channel_from_row).collect()
}

/// Port of `SqlChannelStore.getByName` (channel_store.go:1684), behind the exported `GetByName`
/// (`includeDeleted = false`) and `GetByNameIncludeDeleted` (`true`).
///
/// Same three predicates as [`get_by_names`] — `messageChannelTypes`, the `DeleteAt = 0` that
/// only the non-deleted variant applies, and the team filter — but **the team filter here is not
/// the wildcard.** `getByNames` omits its predicate for an empty team id; `getByName` always
/// writes `TeamId = ? OR TeamId = ''`, so a DM or GM (whose `TeamId` is `""`) is reachable under
/// *any* team's route by name, and an empty team id finds only teamless channels. Two functions
/// one line apart in Go, two different rules, and the difference is on the wire: a DM answers
/// `/teams/{any}/channels/name/{dm-name}` with a 200.
///
/// No `ORDER BY` and `Get` takes the first row — `Name` is unique per team, and a DM name is
/// unique outright, so there is never a second row to pick from.
#[tracing::instrument(skip(pool), fields(team_id = %team_id, name = %name, include_deleted))]
pub async fn get_by_name(
    pool: &PgPool,
    team_id: &str,
    name: &str,
    include_deleted: bool,
) -> Result<Channel, StoreError> {
    let row = sqlx::query_as!(
        ChannelRow,
        r#"
        SELECT c.id,
               c.createat,
               c.updateat,
               c.deleteat,
               c.teamid,
               c.type::text AS "channel_type!",
               c.displayname,
               c.name,
               c.header,
               c.purpose,
               c.lastpostat,
               c.totalmsgcount,
               c.extraupdateat,
               c.creatorid,
               c.schemeid,
               c.groupconstrained,
               c.autotranslation,
               c.shared,
               c.totalmsgcountroot,
               c.lastrootpostat,
               c.bannerinfo,
               c.defaultcategoryname,
               c.discoverable,
               EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp
                    WHERE acp.id = c.id AND acp.type = 'channel'
               ) AS "policy_enforced!",
               COALESCE((
                   SELECT acp.active FROM accesscontrolpolicies acp
                    WHERE acp.id = c.id AND acp.type = 'channel' AND acp.active = TRUE
                    LIMIT 1
               ), false) AS "policy_is_active!"
          FROM channels c
         WHERE c.name = $1
           AND c.type IN ('O', 'P', 'D', 'G')
           AND (c.teamid = $2 OR c.teamid = '')
           AND ($3::boolean OR c.deleteat = 0)
         LIMIT 1
        "#,
        name,
        team_id,
        include_deleted
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find channel with TeamId={team_id} and Name={name}"),
        source,
    })?;

    let Some(row) = row else {
        tracing::Span::current().record("found", false);
        return Err(StoreError::NotFound {
            entity: "Channel",
            criteria: format!("TeamId={team_id}&Name={name}"),
        });
    };
    tracing::Span::current().record("found", true);

    channel_from_row(row)
}

/// Port of `SqlChannelStore.GetChannels` (channel_store.go:1208) — every message channel the
/// user is a member of, scoped to a team, in **`ORDER BY ch.DisplayName`**. That ordering is on
/// the wire: the webapp renders the list in the order it arrives.
///
/// Go builds the `WHERE` incrementally; here each optional predicate is one parameter-guarded
/// disjunct, so there is a single prepared statement to check at compile time. The branches:
///
/// - **The team filter includes `TeamId = ''`.** A DM or GM belongs to no team and appears in
///   *every* team's channel list — that is why the sidebar shows DMs whichever team is open. The
///   predicate is omitted only when `teamId` is empty, which no ported caller passes.
/// - **`IncludeDeleted` without `LastDeleteAt` is no filter at all**; with it, archived channels
///   are kept only if archived at or after that instant (`DeleteAt >= last_delete_at`), living
///   ones always. Without `IncludeDeleted`, `DeleteAt = 0` — and `LastDeleteAt` is ignored.
/// - **`LastUpdateAt > 0`** adds `UpdateAt >= ?`. Not reachable from `getChannelsForTeamForUser`,
///   which never sets it, but it is the same struct and the same statement.
/// - **`Type IN (O, P, D, G)`** — `messageChannelTypes` again.
///
/// **Zero rows is `ErrNotFound`**, not an empty list (channel_store.go:1254). The app layer turns
/// that into a 404 — a member of no channel in the team gets `app.channel.get_channels.not_found`,
/// not `[]`. The `ChannelMembers` join is an inner join written as `FROM Channels ch,
/// ChannelMembers cm` in Go; the same rows either way.
#[tracing::instrument(skip(pool, opts), fields(team_id = %team_id, user_id = %user_id))]
pub async fn get_channels(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
    opts: &ChannelSearchOpts,
) -> Result<ChannelList, StoreError> {
    let rows = sqlx::query_as!(
        ChannelRow,
        r#"
        SELECT ch.id,
               ch.createat,
               ch.updateat,
               ch.deleteat,
               ch.teamid,
               ch.type::text AS "channel_type!",
               ch.displayname,
               ch.name,
               ch.header,
               ch.purpose,
               ch.lastpostat,
               ch.totalmsgcount,
               ch.extraupdateat,
               ch.creatorid,
               ch.schemeid,
               ch.groupconstrained,
               ch.autotranslation,
               ch.shared,
               ch.totalmsgcountroot,
               ch.lastrootpostat,
               ch.bannerinfo,
               ch.defaultcategoryname,
               ch.discoverable,
               EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp
                    WHERE acp.id = ch.id AND acp.type = 'channel'
               ) AS "policy_enforced!",
               COALESCE((
                   SELECT acp.active FROM accesscontrolpolicies acp
                    WHERE acp.id = ch.id AND acp.type = 'channel' AND acp.active = TRUE
                    LIMIT 1
               ), false) AS "policy_is_active!"
          FROM channels ch
          JOIN channelmembers cm ON ch.id = cm.channelid
         WHERE cm.userid = $1
           AND ch.type IN ('O', 'P', 'D', 'G')
           AND ($2::text = '' OR ch.teamid = $2 OR ch.teamid = '')
           AND CASE
                 WHEN $3::boolean THEN ($4::bigint = 0 OR ch.deleteat = 0 OR ch.deleteat >= $4)
                 ELSE ch.deleteat = 0
               END
           AND ($5::bigint <= 0 OR ch.updateat >= $5)
         ORDER BY ch.displayname
        "#,
        user_id,
        team_id,
        opts.include_deleted,
        opts.last_delete_at,
        opts.last_update_at
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get channels with TeamId={team_id} and UserId={user_id}"),
        source,
    })?;

    if rows.is_empty() {
        return Err(StoreError::NotFound {
            entity: "Channel",
            criteria: format!("userId={user_id}"),
        });
    }

    let channels = rows
        .into_iter()
        .map(channel_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChannelList(channels))
}

/// Port of `allChannelMember.Process` (channel_store.go:480).
///
/// **This is not [`get_channel_roles`], and the difference is the whole reason it exists as its own
/// function here rather than being folded into that one.** Both resolve a channel member's
/// effective roles; they disagree on what to do with a scheme role id sitting in the `Roles`
/// column:
///
/// | | `getChannelRoles` (used by `GetMember`) | `Process` (used by `GetAllChannelMembersForUser`) |
/// |---|---|---|
/// | `channel_admin` in `Roles`, `SchemeAdmin` false | sets the flag, drops the literal, re-appends the **scheme's** admin role | leaves `channel_admin` in place, appends nothing |
/// | position of scheme roles | always last, after the explicit ones | wherever the column put them |
///
/// So a member holding a literal `channel_admin` in a channel with a scheme gets the *scheme's*
/// admin role from `GetMember` and the literal `channel_admin` from here. Different role names
/// reach `RolesGrantPermission`, and they can carry different permissions. This is Go's behaviour
/// in both cases, and the permission checks read **this** one — see [D-142].
#[allow(clippy::too_many_arguments)] // Go's signature; splitting it would obscure the porting map.
pub fn process_all_channel_member_roles(
    scheme_guest: bool,
    scheme_user: bool,
    scheme_admin: bool,
    default_team_guest_role: &str,
    default_team_user_role: &str,
    default_team_admin_role: &str,
    default_channel_guest_role: &str,
    default_channel_user_role: &str,
    default_channel_admin_role: &str,
    roles: &str,
) -> String {
    // Go keeps `strings.Fields(db.Roles)` verbatim — no scheme id is recognised or removed.
    let mut result: Vec<&str> = roles.split_whitespace().collect();

    fn implied<'a>(channel_default: &'a str, team_default: &'a str, constant: &'a str) -> &'a str {
        if !channel_default.is_empty() {
            channel_default
        } else if !team_default.is_empty() {
            team_default
        } else {
            constant
        }
    }

    let mut implied_roles: Vec<&str> = Vec::new();
    if scheme_guest {
        implied_roles.push(implied(
            default_channel_guest_role,
            default_team_guest_role,
            CHANNEL_GUEST_ROLE_ID,
        ));
    }
    if scheme_user {
        implied_roles.push(implied(
            default_channel_user_role,
            default_team_user_role,
            CHANNEL_USER_ROLE_ID,
        ));
    }
    if scheme_admin {
        implied_roles.push(implied(
            default_channel_admin_role,
            default_team_admin_role,
            CHANNEL_ADMIN_ROLE_ID,
        ));
    }

    for implied_role in implied_roles {
        if !result.contains(&implied_role) {
            result.push(implied_role);
        }
    }

    result.join(" ")
}

/// Port of `SqlChannelStore.GetAllChannelMembersForUser` (channel_store.go:2527).
///
/// Returns channel id → effective role names, for **every** channel the user is a member of.
///
/// Go's comment at the one call site that matters says why the permission checks use this rather
/// than `GetMember`: "We call GetAllChannelMembersForUser instead of just getting a single member
/// from the DB, because it's cache backed and this is a very frequent call"
/// (authorization.go:335). **This port has no cache** — the standing "Rust reads through" decision
/// from the vertical slice ([D-087]) — so we pay a full scan of the user's memberships per check
/// where Go pays one map lookup. Correct, and slower; see [D-143].
///
/// `allowFromCache` is Go's parameter and is dropped here rather than accepted and ignored: with no
/// cache there is nothing for it to select, and a parameter that does nothing is a lie at the call
/// site. `includeDeleted` is real and kept — it drops the `Channels.DeleteAt = 0` filter.
#[tracing::instrument(skip(pool), fields(user_id = %user_id))]
pub async fn get_all_channel_members_for_user(
    pool: &PgPool,
    user_id: &str,
    include_deleted: bool,
) -> Result<HashMap<String, String>, StoreError> {
    // Go builds the `DeleteAt` predicate conditionally; expressed inside one static statement so
    // sqlx keeps checking it at compile time. Note this query's `Channels` join is Go's `Join(...)`
    // — an INNER join, same as `GetMember`'s — so an orphaned membership is invisible here too.
    let rows = sqlx::query!(
        r#"
        SELECT cm.channelid,
               cm.roles,
               cm.schemeguest,
               cm.schemeuser,
               cm.schemeadmin,
               teamscheme.defaultchannelguestrole    AS teamschemedefaultguestrole,
               teamscheme.defaultchanneluserrole     AS teamschemedefaultuserrole,
               teamscheme.defaultchanneladminrole    AS teamschemedefaultadminrole,
               channelscheme.defaultchannelguestrole AS channelschemedefaultguestrole,
               channelscheme.defaultchanneluserrole  AS channelschemedefaultuserrole,
               channelscheme.defaultchanneladminrole AS channelschemedefaultadminrole
          FROM channelmembers cm
          INNER JOIN channels c ON cm.channelid = c.id
          LEFT JOIN schemes channelscheme ON c.schemeid = channelscheme.id
          LEFT JOIN teams t ON c.teamid = t.id
          LEFT JOIN schemes teamscheme ON t.schemeid = teamscheme.id
         WHERE cm.userid = $1
           AND ($2 OR c.deleteat = 0)
        "#,
        user_id,
        include_deleted
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to find ChannelMembers, TeamScheme and ChannelScheme data".to_owned(),
        source,
    })?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let roles = process_all_channel_member_roles(
                row.schemeguest.unwrap_or_default(),
                row.schemeuser.unwrap_or_default(),
                row.schemeadmin.unwrap_or_default(),
                row.teamschemedefaultguestrole
                    .as_deref()
                    .unwrap_or_default(),
                row.teamschemedefaultuserrole.as_deref().unwrap_or_default(),
                row.teamschemedefaultadminrole
                    .as_deref()
                    .unwrap_or_default(),
                row.channelschemedefaultguestrole
                    .as_deref()
                    .unwrap_or_default(),
                row.channelschemedefaultuserrole
                    .as_deref()
                    .unwrap_or_default(),
                row.channelschemedefaultadminrole
                    .as_deref()
                    .unwrap_or_default(),
                row.roles.as_deref().unwrap_or_default(),
            );
            (row.channelid, roles)
        })
        .collect())
}

/// Port of `SqlChannelStore.GetChannelUnread` (channel_store.go:921).
///
/// # The unqualified column names are not ambiguous, and which table wins is the behaviour
///
/// Go writes `FROM Channels, ChannelMembers` — an implicit cross join — and then four predicates
/// that name their columns **bare**: `Id = ChannelId`, `Id = ?`, `UserId = ?`, `DeleteAt = 0`.
/// Each resolves to whichever table actually has that column, and only one does in every case:
/// `ChannelMembers` has no `Id` and no `DeleteAt` (its key is `(ChannelId, UserId)`), `Channels`
/// has no `ChannelId` and no `UserId`. So the join is `Channels.Id = ChannelMembers.ChannelId`
/// and — the load-bearing one — **`DeleteAt` is the channel's**.
///
/// That makes this the opposite of `GetMember`, which takes `includeDeleted` and whose api4 call
/// site passes it: a member of an **archived** channel still reads back from `GetMember`, but
/// `GetChannelUnread` finds nothing and the app layer turns that into a 404. Two routes, the same
/// two ids, different answers — see the module notes in `MIGRATION.md`.
///
/// # Three more things the query decides
///
/// - **`Type IN (O, P, D, G)`** — `messageChannelTypes` again (channel_store.go:38), same as
///   [`get`]. A board has unread counters in the schema and is deliberately unreachable here.
/// - **`MsgCount` is a subtraction, not a column.** `Channels.TotalMsgCount -
///   ChannelMembers.MsgCount` is "how many messages arrived since this member last caught up",
///   and nothing constrains it to be non-negative: a member whose `MsgCount` was written ahead of
///   the channel's total reads back negative, and Go passes that straight to the client.
/// - **Only `UrgentMentionCount` is coalesced.** The other six selected values are scanned into
///   plain `int64`/`string` fields of `model.ChannelUnread`, so a NULL in any of them is a Go
///   *scan* error and a 500 — not a zero. The `!` overrides below reproduce exactly that: sqlx
///   raises `ColumnDecode` where Go raises `converting NULL to int64 is unsupported`. Using
///   `unwrap_or_default` here — the convention [`get_member`] follows, because Go's row struct
///   there really is built out of `sql.Null*` — would answer `0` where Go answers 500.
///
/// `NotifyProps` carries `json:"-"`, so it never reaches a client. It is selected because the
/// **app** layer branches on it: `mark_unread = mention` zeroes the two message counts
/// (channel.go:2712).
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id, user_id = %user_id))]
pub async fn get_channel_unread(
    pool: &PgPool,
    channel_id: &str,
    user_id: &str,
) -> Result<ChannelUnread, StoreError> {
    let row = sqlx::query!(
        r#"
        SELECT channels.teamid AS "teamid!",
               channels.id AS "channelid!",
               (channels.totalmsgcount - channelmembers.msgcount) AS "msgcount!",
               (channels.totalmsgcountroot - channelmembers.msgcountroot) AS "msgcountroot!",
               channelmembers.mentioncount AS "mentioncount!",
               channelmembers.mentioncountroot AS "mentioncountroot!",
               COALESCE(channelmembers.urgentmentioncount, 0) AS "urgentmentioncount!",
               channelmembers.notifyprops
          FROM channels, channelmembers
         WHERE channels.id = channelmembers.channelid
           AND channels.id = $1
           AND channelmembers.userid = $2
           AND channels.deleteat = 0
           AND channels.type IN ('O', 'P', 'D', 'G')
        "#,
        channel_id,
        user_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get Channel with channelId={channel_id} and userId={user_id}"),
        source,
    })?;

    let Some(row) = row else {
        tracing::Span::current().record("found", false);
        // Go's entity here is **`Channel`**, not `ChannelUnread` and not `ChannelMember`
        // (channel_store.go:945). Only the app layer's error id reaches a client, so this is
        // invisible on the wire — but it is what a `store.ErrNotFound` says in a log line.
        return Err(StoreError::NotFound {
            entity: "Channel",
            criteria: format!("channelId={channel_id},userId={user_id}"),
        });
    };
    tracing::Span::current().record("found", true);

    // Same jsonb split as [`get_member`]: SQL NULL and the JSON value `null` are different rows,
    // and Go's `json.Unmarshal` turns the latter into a nil map rather than an error ([D-135]).
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
        msg_count_root: row.msgcountroot,
        mention_count: row.mentioncount,
        mention_count_root: row.mentioncountroot,
        urgent_mention_count: row.urgentmentioncount,
        notify_props,
    })
}

/// Port of `SqlChannelStore.GetMemberCount` (channel_store.go:2666).
///
/// The join with `Users` is the behaviour: `Users.DeleteAt = 0` means a **deactivated** member's
/// row in `ChannelMembers` — which survives deactivation — does not count. A bare count over
/// `ChannelMembers` alone would drift upward by exactly the members nobody can see any more.
///
/// A channel id that matches nothing is a count of `0`, not an error — `COUNT(*)` has no
/// not-found case, and neither does Go's.
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id))]
pub async fn get_member_count(pool: &PgPool, channel_id: &str) -> Result<i64, StoreError> {
    sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
          FROM channelmembers, users
         WHERE channelmembers.userid = users.id
           AND channelmembers.channelid = $1
           AND users.deleteat = 0
        "#,
        channel_id
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to count ChannelMembers with channelId={channel_id}"),
        source,
    })
}

/// Port of `SqlChannelStore.GetGuestCount` (channel_store.go:2752).
///
/// [`get_member_count`]'s query plus one predicate: `SchemeGuest = TRUE`. The column is nullable,
/// and `NULL = TRUE` is SQL-`NULL`, so a member whose flag was never written counts as **not** a
/// guest on both servers — the predicate, not a `COALESCE`, carries that.
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id))]
pub async fn get_guest_count(pool: &PgPool, channel_id: &str) -> Result<i64, StoreError> {
    sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
          FROM channelmembers, users
         WHERE channelmembers.userid = users.id
           AND channelmembers.channelid = $1
           AND channelmembers.schemeguest = TRUE
           AND users.deleteat = 0
        "#,
        channel_id
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to count Guests with channelId={channel_id}"),
        source,
    })
}

/// Port of `SqlChannelStore.GetPinnedPostCount` (channel_store.go:2731).
///
/// `DeleteAt = 0` here is the **post's** — a deleted post stays pinned in its row, and counting
/// it would advertise a pin nobody can open.
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id))]
pub async fn get_pinned_post_count(pool: &PgPool, channel_id: &str) -> Result<i64, StoreError> {
    sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
          FROM posts
         WHERE ispinned = true
           AND channelid = $1
           AND deleteat = 0
        "#,
        channel_id
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to count pinned Posts with channelId={channel_id}"),
        source,
    })
}

/// Port of `SqlChannelStore.GetFileCount` (channel_store.go:2646).
///
/// `PostId != ''` is the predicate a reader would drop as redundant, and it is not: a file
/// uploaded but never attached to a post has a `FileInfo` row with an empty `PostId`, and Go does
/// not count it. `DeleteAt = 0` is the **file's** own, not its post's — deleting a post also
/// tombstones its `FileInfo` rows, which is the path that makes the predicate reachable over
/// REST.
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id))]
pub async fn get_file_count(pool: &PgPool, channel_id: &str) -> Result<i64, StoreError> {
    sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
          FROM fileinfo
         WHERE fileinfo.deleteat = 0
           AND fileinfo.postid != ''
           AND fileinfo.channelid = $1
        "#,
        channel_id
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to count files with channelId={channel_id}"),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case here was **measured** against the running Go server rather than reasoned about:
    /// `crates/mm-store/tests/db_channel_members.rs` drives the same shapes through
    /// `GET /api/v4/channels/{id}/members/{id}` and this function, and asserts they agree. These
    /// unit tests pin the answers so a regression fails without Docker.
    ///
    /// The row in the development database — `schemeuser`, `schemeadmin`, no scheme, empty
    /// `Roles` — and the exact body Go returns for it:
    ///
    /// ```json
    /// {"roles":"channel_user channel_admin","scheme_guest":false,"scheme_user":true,
    ///  "scheme_admin":true,"explicit_roles":""}
    /// ```
    #[test]
    fn matches_the_running_go_server_for_a_scheme_less_channel() {
        let info = get_channel_roles(false, true, true, "", "", "", "", "", "", "");
        assert_eq!(info.roles.join(" "), "channel_user channel_admin");
        assert_eq!(info.explicit_roles.join(" "), "");
        assert!(!info.scheme_guest);
        assert!(info.scheme_user);
        assert!(info.scheme_admin);
    }

    /// The un-migrated case Go's comment describes, measured against Go with the shared row's
    /// `Roles` column set to `custom_one channel_guest custom_two`:
    ///
    /// ```json
    /// {"roles":"custom_one custom_two channel_guest channel_user channel_admin",
    ///  "explicit_roles":"custom_one custom_two","scheme_guest":true}
    /// ```
    ///
    /// Three things at once: `channel_guest` in the column **sets the flag the column denied**,
    /// it is kept out of `explicit_roles`, and the explicit roles keep their relative order ahead
    /// of every implied one.
    #[test]
    fn a_scheme_role_in_the_roles_column_sets_the_flag_and_is_not_explicit() {
        let info = get_channel_roles(
            false,
            true,
            true,
            "",
            "",
            "",
            "",
            "",
            "",
            "custom_one channel_guest custom_two",
        );
        assert!(
            info.scheme_guest,
            "the column said false; the role says true"
        );
        assert_eq!(info.explicit_roles, vec!["custom_one", "custom_two"]);
        assert_eq!(
            info.roles.join(" "),
            "custom_one custom_two channel_guest channel_user channel_admin"
        );
    }

    /// The channel scheme's defaults replace the constants. Measured with the shared channel
    /// pointed at a scheme whose channel roles are `mmrs_cs_*`:
    /// `{"roles":"mmrs_cs_channel_user mmrs_cs_channel_admin"}`.
    #[test]
    fn the_channel_scheme_replaces_the_constants() {
        let info = get_channel_roles(
            false, true, true, "", "", "", "cs_guest", "cs_user", "cs_admin", "",
        );
        assert_eq!(info.roles.join(" "), "cs_user cs_admin");
    }

    /// With **no** channel scheme, the team scheme's `DefaultChannel*Role` columns are the
    /// fallback. Measured with the shared team pointed at a scheme whose channel roles are
    /// `mmrs_ts_*` and the channel's `SchemeId` NULL:
    /// `{"roles":"mmrs_ts_channel_user mmrs_ts_channel_admin"}`.
    #[test]
    fn the_team_scheme_is_the_fallback_when_the_channel_has_none() {
        let info = get_channel_roles(
            false, true, true, "ts_guest", "ts_user", "ts_admin", "", "", "", "",
        );
        assert_eq!(info.roles.join(" "), "ts_user ts_admin");
    }

    /// **The channel scheme wins when both are present.** Measured with both schemes attached at
    /// once: Go answered `mmrs_cs_channel_user mmrs_cs_channel_admin`, the channel scheme's names.
    ///
    /// This is the branch that distinguishes this function from
    /// [`crate::team_store::get_team_roles`], and getting it backwards is a silent permission
    /// difference rather than an error.
    #[test]
    fn the_channel_scheme_beats_the_team_scheme() {
        let info = get_channel_roles(
            true, true, true, "ts_guest", "ts_user", "ts_admin", "cs_guest", "cs_user", "cs_admin",
            "",
        );
        assert_eq!(info.roles.join(" "), "cs_guest cs_user cs_admin");
    }

    /// The fallback is per role, not per scheme: a team scheme that names only a user role and a
    /// channel scheme that names only an admin role each supply their own level, and the guest
    /// role — named by neither — falls all the way through to the constant.
    #[test]
    fn each_role_falls_back_independently() {
        let info = get_channel_roles(true, true, true, "", "ts_user", "", "", "", "cs_admin", "");
        assert_eq!(info.roles.join(" "), "channel_guest ts_user cs_admin");
    }

    /// Only the flags that are set contribute, and the order is always guest, user, admin —
    /// never the order the flags were discovered in.
    #[test]
    fn implied_roles_are_emitted_in_guest_user_admin_order() {
        let info = get_channel_roles(true, false, true, "", "", "", "", "", "", "");
        assert_eq!(info.roles.join(" "), "channel_guest channel_admin");
    }

    /// The dedup check reads `roles` as it grows, so two scheme defaults with the same name
    /// collapse. Reproduced because it is Go's behaviour, not because it is desirable.
    #[test]
    fn identical_scheme_defaults_collapse_to_one_role() {
        let info = get_channel_roles(
            false,
            true,
            true,
            "",
            "",
            "",
            "",
            "same_role",
            "same_role",
            "",
        );
        assert_eq!(info.roles.join(" "), "same_role");
        assert!(info.scheme_user && info.scheme_admin, "both flags stay set");
    }

    /// An implied role already present as an explicit role is not appended twice.
    #[test]
    fn an_implied_role_already_explicit_is_not_duplicated() {
        let info = get_channel_roles(
            false,
            true,
            false,
            "",
            "",
            "",
            "",
            "custom_user",
            "",
            "custom_user",
        );
        assert_eq!(info.roles.join(" "), "custom_user");
        assert_eq!(info.explicit_roles, vec!["custom_user"]);
    }

    /// `strings.Fields` drops empty fields, so runs of whitespace and a blank column both yield
    /// nothing. A NULL column reaches this as `""` via `unwrap_or_default`.
    #[test]
    fn whitespace_only_roles_contribute_nothing() {
        for input in ["", "   ", "\t\n ", "  \t"] {
            let info = get_channel_roles(false, false, false, "", "", "", "", "", "", input);
            assert!(
                info.roles.is_empty() && info.explicit_roles.is_empty(),
                "input {input:?} should contribute no roles"
            );
        }

        let info = get_channel_roles(false, false, false, "", "", "", "", "", "", "  a \t\n b  ");
        assert_eq!(info.explicit_roles, vec!["a", "b"]);
    }

    /// No flags and no roles is an empty result, not a defaulted one.
    #[test]
    fn nothing_set_yields_nothing() {
        let info = get_channel_roles(false, false, false, "", "", "", "", "", "", "");
        assert_eq!(info, RolesInfo::default());
        assert_eq!(info.roles.join(" "), "");
    }

    /// The team-scoped role ids are **not** recognised here. `team_admin` sitting in a
    /// `ChannelMembers.Roles` column stays an explicit role and sets no flag — the mirror of
    /// [`crate::team_store::get_team_roles`], which ignores `channel_admin` the same way.
    #[test]
    fn team_role_ids_are_explicit_roles_to_a_channel_member() {
        let info = get_channel_roles(
            false,
            false,
            false,
            "",
            "",
            "",
            "",
            "",
            "",
            "team_admin team_user",
        );
        assert_eq!(info.explicit_roles, vec!["team_admin", "team_user"]);
        assert!(!info.scheme_admin && !info.scheme_user);
    }

    // ---------------------------------------------------------------------
    // process_all_channel_member_roles — the *other* resolver
    // ---------------------------------------------------------------------

    /// Convenience: the no-scheme case, which is every channel on Team Edition.
    fn process(guest: bool, user: bool, admin: bool, roles: &str) -> String {
        process_all_channel_member_roles(guest, user, admin, "", "", "", "", "", "", roles)
    }

    /// **The divergence, pinned.** Same row, two Go functions, two different answers — and it is
    /// not a subtlety of naming, it changes which permissions apply.
    ///
    /// Measured against the running Go server on 2026-08-19. A channel pointed at a scheme whose
    /// `DefaultChannelUserRole` is `mmrs_dv2_channel_user` (a copy of `channel_user` with
    /// `read_channel` removed), and a member whose `Roles` column is the literal `channel_user`
    /// with every scheme flag false:
    ///
    /// - `GET /channels/{id}/members/{uid}` — the `GetMember` path — reported
    ///   `"roles": "mmrs_dv2_channel_user"`.
    /// - The **permission check on that same request** granted, returning **200**, which it could
    ///   only do by resolving the member's roles to `channel_user`.
    ///
    /// So Go told the client the member holds a role that does not grant `read_channel`, while
    /// simultaneously granting `read_channel` on the strength of a different role name. Both
    /// behaviours are reproduced, separately, because a port that unified them would change one of
    /// the two answers. See [D-142].
    #[test]
    fn process_and_get_channel_roles_disagree_about_a_literal_scheme_id() {
        // The `GetMember` path: the literal sets the flag, is dropped, and the scheme's name is
        // appended in its place.
        let via_get_member = get_channel_roles(
            false,
            false,
            false,
            "",
            "",
            "",
            "",
            "scheme_user_role",
            "",
            "channel_user",
        );
        assert_eq!(via_get_member.roles.join(" "), "scheme_user_role");
        assert!(via_get_member.scheme_user);

        // The permission path: the literal survives untouched and no scheme role is implied,
        // because the flag on the row is still false.
        let via_permission_check = process_all_channel_member_roles(
            false,
            false,
            false,
            "",
            "",
            "",
            "",
            "scheme_user_role",
            "",
            "channel_user",
        );
        assert_eq!(via_permission_check, "channel_user");

        assert_ne!(
            via_get_member.roles.join(" "),
            via_permission_check,
            "if these ever agree, one of the two ports has drifted"
        );
    }

    /// `Process` does not recognise the scheme role ids at all: no flag is set and nothing is
    /// removed, which is the whole of the difference above stated positively.
    #[test]
    fn process_keeps_scheme_role_ids_verbatim_and_in_place() {
        assert_eq!(
            process(false, false, false, "channel_admin custom_one channel_user"),
            "channel_admin custom_one channel_user"
        );
    }

    /// With the flags set and no scheme, the constants are appended after whatever the column
    /// held — guest, user, admin, in that order.
    #[test]
    fn implied_constants_are_appended_in_guest_user_admin_order() {
        assert_eq!(
            process(true, true, true, "custom_one"),
            "custom_one channel_guest channel_user channel_admin"
        );
    }

    /// The dedup is against the column's contents too, so a flag whose implied role is already
    /// present adds nothing — this is the one case where `Process` and `getChannelRoles` land on
    /// the same string by different routes.
    #[test]
    fn an_implied_role_already_in_the_column_is_not_duplicated() {
        assert_eq!(process(false, true, false, "channel_user"), "channel_user");
        assert_eq!(
            get_channel_roles(false, true, false, "", "", "", "", "", "", "channel_user")
                .roles
                .join(" "),
            "channel_user"
        );
    }

    /// The same three-level fallback as `get_channel_roles`: channel scheme, then team scheme,
    /// then the constant, resolved independently per role.
    #[test]
    fn process_falls_back_channel_then_team_then_constant() {
        assert_eq!(
            process_all_channel_member_roles(
                true, true, true, "", "ts_user", "", "", "", "cs_admin", ""
            ),
            "channel_guest ts_user cs_admin"
        );
    }

    /// Whitespace runs collapse and a blank column contributes nothing, so a member with no roles
    /// and no flags resolves to the empty string rather than to a role named `""`.
    #[test]
    fn process_handles_blank_and_padded_columns() {
        assert_eq!(process(false, false, false, "   \t "), "");
        assert_eq!(process(false, false, false, ""), "");
        assert_eq!(
            process(false, true, false, "  a \t b  "),
            "a b channel_user"
        );
    }

    /// Go's not-found message embeds both ids; neither is a credential, so it is reproduced.
    #[test]
    fn channel_member_not_found_carries_both_ids() {
        let err = StoreError::NotFound {
            entity: "ChannelMember",
            criteria: "channelId=abc, userId=def".to_owned(),
        };
        assert!(err.is_not_found());
        assert_eq!(
            err.to_string(),
            "ChannelMember not found: channelId=abc, userId=def"
        );
    }
}
