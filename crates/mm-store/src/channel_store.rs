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

use std::collections::{BTreeMap, HashMap};

use mm_model::channel::{CHANNEL_TYPE_DIRECT, Channel, ChannelBannerInfo, ChannelSearchOpts};
use mm_model::channel_list::ChannelList;
use mm_model::channel_member::{
    CHANNEL_NOTIFY_DEFAULT, ChannelMember, ChannelMemberWithTeamData, ChannelMembersWithTeamData,
    ChannelUnread,
};
use mm_model::post_list::PostList;
use mm_model::role::{CHANNEL_ADMIN_ROLE_ID, CHANNEL_GUEST_ROLE_ID, CHANNEL_USER_ROLE_ID};
use mm_model::user::{PUSH_NOTIFY_PROP, USER_NOTIFY_ALL, USER_NOTIFY_MENTION};
use mm_model::utils::StringMap;
use sqlx::PgPool;

use crate::error::StoreError;
use crate::post_store::{PostRow, post_from_row};
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
    /// Port of `SqlChannelStore.DeleteSidebarChannelsByPreferences`
    /// (channel_store_categories.go:953).
    ///
    /// Unfavouriting a channel has to take it out of the sidebar's Favorites category as well as
    /// out of `Preferences`, or the channel keeps appearing there. Go loops the batch inside one
    /// transaction, **skipping every preference whose category is not `favorite_channel`** — so a
    /// batch of ordinary preferences opens and commits an empty transaction, which is reproduced
    /// rather than short-circuited.
    fn delete_sidebar_channels_by_preferences(
        &self,
        preferences: &[(String, String)],
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlChannelStore.Get` (channel_store.go:985).
    fn get(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<Channel, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetChannelsByScheme` (channel_store.go:4071).
    fn get_channels_by_scheme(
        &self,
        scheme_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<Channel>, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetForPost` (channel_store.go:3152).
    fn get_for_post(
        &self,
        post_id: &str,
    ) -> impl std::future::Future<Output = Result<Channel, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetMemberForPost` (channel_store.go:2479).
    fn get_member_for_post(
        &self,
        post_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<ChannelMember, StoreError>> + Send;

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

    /// Port of `SqlChannelStore.GetMembersForUserWithPagination` (channel_store.go:3285): the
    /// caller's memberships across every team, page/offset paginated.
    fn get_members_for_user_with_pagination(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> impl std::future::Future<Output = Result<ChannelMembersWithTeamData, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetMembersForUserWithCursorPagination` (channel_store.go:3299):
    /// the same list walked by a `ChannelId >` cursor, and `ErrNotFound` when the page is empty.
    fn get_members_for_user_with_cursor_pagination(
        &self,
        user_id: &str,
        per_page: i64,
        from_channel_id: &str,
    ) -> impl std::future::Future<Output = Result<ChannelMembersWithTeamData, StoreError>> + Send;

    /// Port of `SqlChannelStore.AutocompleteInTeamForSearch` (channel_store.go:3464), including
    /// the direct-message pass it appends and the sort that merges the two.
    fn autocomplete_in_team_for_search(
        &self,
        team_id: &str,
        user_id: &str,
        term: &str,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.AutocompleteInTeam` (channel_store.go:3443) through the
    /// `buildAutocompleteInTeamQuery` (:3405) and `performSearch` (:3904) it is made of.
    /// `include_deleted` is not a parameter: its only caller passes `true`.
    fn autocomplete_in_team(
        &self,
        team_id: &str,
        user_id: &str,
        term: &str,
        is_guest: bool,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.SearchInTeam` (channel_store.go:3598).
    fn search_in_team(
        &self,
        team_id: &str,
        term: &str,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.SearchForUserInTeam` (channel_store.go:3620).
    fn search_for_user_in_team(
        &self,
        user_id: &str,
        team_id: &str,
        term: &str,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetChannelsMemberCount` (channel_store.go:2573).
    fn get_channels_member_count(
        &self,
        ids: &[String],
    ) -> impl std::future::Future<Output = Result<BTreeMap<String, i64>, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetMany` (channel_store.go:1043): [`ChannelStore::get`]'s
    /// query with an id **list**, and the same `ErrNotFound` when nothing matches.
    fn get_many(
        &self,
        ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<Channel>, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetChannels` (channel_store.go:1208): the channels of one user
    /// in one team, display-name order, `ErrNotFound` when there are none.
    fn get_channels(
        &self,
        team_id: &str,
        user_id: &str,
        opts: &ChannelSearchOpts,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetChannelsByUser` (channel_store.go:1264): one page of every
    /// message channel the user is a member of, across all teams, in **id order** — the keyset
    /// the streaming `getChannelsForUser` handler walks 100 at a time. `ErrNotFound` for an
    /// empty page, which the handler relies on to stop when the page size divides the total.
    fn get_channels_by_user(
        &self,
        user_id: &str,
        include_deleted: bool,
        last_delete_at: i64,
        page_size: i64,
        from_channel_id: &str,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetPublicChannelsForTeam` (channel_store.go:1499): one
    /// offset/limit page of a team's living public channels, joined through the denormalised
    /// `PublicChannels` shadow table.
    fn get_public_channels_for_team(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetPublicChannelsByIdsForTeam` (channel_store.go:1527): a named
    /// set of a team's living public channels, display-name order, `ErrNotFound` when none of
    /// the ids match.
    fn get_public_channels_by_ids_for_team(
        &self,
        team_id: &str,
        channel_ids: &[String],
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetPrivateChannelsForTeam` (channel_store.go:1476): one
    /// offset/limit page of a team's living private channels, straight off `Channels`.
    fn get_private_channels_for_team(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetDeleted` (channel_store.go:1735): one offset/limit page of a
    /// team's **archived** message channels, narrowed to what `user_id` may see unless
    /// `skip_team_membership_check`.
    fn get_deleted(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
        user_id: &str,
        skip_team_membership_check: bool,
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

    /// Port of `SqlChannelStore.GetMembersByIds` (channel_store.go:3992): the memberships of a
    /// named set of users in one channel, unpaginated and unordered. Zero matches is an empty
    /// list, not a miss.
    fn get_members_by_ids(
        &self,
        channel_id: &str,
        user_ids: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<ChannelMember>, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetMembersForUser` (channel_store.go:3261): every membership
    /// of one user in one team's channels, plus the teamless ones.
    fn get_members_for_user(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<ChannelMember>, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetMemberLastViewedAt` (channel_store.go:2462).
    ///
    /// A single scalar rather than [`ChannelStore::get_member`], because Go reads it that way and
    /// the difference is observable: `GetMember` computes scheme roles through two joins and
    /// raises `MissingChannelMemberError` for a missing row, while this reads one `COALESCE`d
    /// column and raises `LastViewedAt` not-found. The app layers above them report **different
    /// error ids** as a result.
    fn get_member_last_viewed_at(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetChannelMembersTimezones` (channel_store.go:2214).
    ///
    /// Returns the raw `Users.Timezone` maps, one per member row, **unfiltered and
    /// undeduplicated** — the app layer does both. A `LEFT JOIN`, so a membership whose user row
    /// is gone contributes a NULL that becomes an empty map here.
    fn get_channel_members_timezones(
        &self,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<StringMap>, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetPinnedPosts` (channel_store.go:959).
    ///
    /// A `Posts` query living on the **channel** store, which is Go's placement and not an
    /// accident of this port — `getPinnedPosts` reaches it through `App.GetPinnedPosts`
    /// (app/channel.go:3992), not through the post store.
    fn get_pinned_posts(
        &self,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<PostList, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetChannelsWithUnreadsAndWithMentions` (channel_store.go:2232).
    fn get_channels_with_unreads_and_with_mentions(
        &self,
        channel_ids: &[String],
        user_id: &str,
        user_notify_props: Option<&StringMap>,
    ) -> impl std::future::Future<Output = Result<UnreadsAndMentions, StoreError>> + Send;

    /// Port of `SqlChannelStore.UpdateLastViewedAt` (channel_store.go:2838).
    fn update_last_viewed_at(
        &self,
        channel_ids: &[String],
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<BTreeMap<String, i64>, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetBoardChannel` (channel_store.go:1003).
    fn get_board_channel(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<Channel, StoreError>> + Send;
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
    /// `preferences` is the `(user_id, channel_id)` pairs of the favourite-channel preferences in
    /// the batch — the caller has already applied Go's category filter, since it is the only
    /// thing the category is used for here.
    ///
    /// The DELETE joins `SidebarCategories` to scope the removal to the **Favorites** category:
    /// a channel the user also placed in a custom category stays there. Dropping the join would
    /// remove it from every category the user has.
    #[tracing::instrument(skip_all, fields(pairs = preferences.len()))]
    async fn delete_sidebar_channels_by_preferences(
        &self,
        preferences: &[(String, String)],
    ) -> Result<(), StoreError> {
        if preferences.is_empty() {
            return Ok(());
        }

        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "DeleteSidebarChannelsByPreferences: begin_transaction".to_owned(),
            source,
        })?;

        for (user_id, channel_id) in preferences {
            sqlx::query!(
                r#"
                DELETE FROM sidebarchannels
                 USING sidebarcategories
                 WHERE sidebarchannels.categoryid = sidebarcategories.id
                   AND sidebarchannels.userid = $1
                   AND sidebarchannels.channelid = $2
                   AND sidebarcategories.type = $3
                "#,
                user_id,
                channel_id,
                mm_model::sidebar_category::SIDEBAR_CATEGORY_FAVORITES,
            )
            .execute(&mut *tx)
            .await
            .map_err(|source| StoreError::Db {
                context: "Failed to remove sidebar entries for preference".to_owned(),
                source,
            })?;
        }

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "DeleteSidebarChannelsByPreferences: commit_transaction".to_owned(),
            source,
        })
    }

    #[tracing::instrument(skip_all, fields(scheme_id = %scheme_id, offset, limit, found))]
    async fn get_channels_by_scheme(
        &self,
        scheme_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<Channel>, StoreError> {
        get_channels_by_scheme(&self.pool, scheme_id, offset, limit).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %id, found))]
    async fn get(&self, id: &str) -> Result<Channel, StoreError> {
        get(&self.pool, id).await
    }

    #[tracing::instrument(skip_all, fields(asked = ids.len(), found))]
    async fn get_many(&self, ids: &[String]) -> Result<Vec<Channel>, StoreError> {
        get_many(&self.pool, ids).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, found))]
    async fn search_in_team(&self, team_id: &str, term: &str) -> Result<ChannelList, StoreError> {
        search_in_team(&self.pool, team_id, term).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, found))]
    async fn search_for_user_in_team(
        &self,
        user_id: &str,
        team_id: &str,
        term: &str,
    ) -> Result<ChannelList, StoreError> {
        search_for_user_in_team(&self.pool, user_id, team_id, term).await
    }

    #[tracing::instrument(skip_all, fields(asked = ids.len(), counted))]
    async fn get_channels_member_count(
        &self,
        ids: &[String],
    ) -> Result<BTreeMap<String, i64>, StoreError> {
        get_channels_member_count(&self.pool, ids).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, is_guest, found))]
    async fn autocomplete_in_team(
        &self,
        team_id: &str,
        user_id: &str,
        term: &str,
        is_guest: bool,
    ) -> Result<ChannelList, StoreError> {
        autocomplete_in_team(&self.pool, team_id, user_id, term, is_guest).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, page, per_page, found))]
    async fn get_members_for_user_with_pagination(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> Result<ChannelMembersWithTeamData, StoreError> {
        get_members_for_user_with_pagination(&self.pool, user_id, page, per_page).await
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, per_page, found))]
    async fn get_members_for_user_with_cursor_pagination(
        &self,
        user_id: &str,
        per_page: i64,
        from_channel_id: &str,
    ) -> Result<ChannelMembersWithTeamData, StoreError> {
        get_members_for_user_with_cursor_pagination(&self.pool, user_id, per_page, from_channel_id)
            .await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, found))]
    async fn autocomplete_in_team_for_search(
        &self,
        team_id: &str,
        user_id: &str,
        term: &str,
    ) -> Result<ChannelList, StoreError> {
        autocomplete_in_team_for_search(&self.pool, team_id, user_id, term).await
    }

    #[tracing::instrument(skip_all, fields(post_id = %post_id, found))]
    async fn get_for_post(&self, post_id: &str) -> Result<Channel, StoreError> {
        get_for_post(&self.pool, post_id).await
    }

    #[tracing::instrument(skip_all, fields(post_id = %post_id, user_id = %user_id, found))]
    async fn get_member_for_post(
        &self,
        post_id: &str,
        user_id: &str,
    ) -> Result<ChannelMember, StoreError> {
        get_member_for_post(&self.pool, post_id, user_id).await
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

    async fn get_channels_with_unreads_and_with_mentions(
        &self,
        channel_ids: &[String],
        user_id: &str,
        user_notify_props: Option<&StringMap>,
    ) -> Result<UnreadsAndMentions, StoreError> {
        get_channels_with_unreads_and_with_mentions(
            &self.pool,
            channel_ids,
            user_id,
            user_notify_props,
        )
        .await
    }

    async fn update_last_viewed_at(
        &self,
        channel_ids: &[String],
        user_id: &str,
    ) -> Result<BTreeMap<String, i64>, StoreError> {
        update_last_viewed_at(&self.pool, channel_ids, user_id).await
    }

    async fn get_board_channel(&self, id: &str) -> Result<Channel, StoreError> {
        get_board_channel(&self.pool, id).await
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

    #[tracing::instrument(skip_all, fields(user_id = %user_id, from_channel_id = %from_channel_id))]
    async fn get_channels_by_user(
        &self,
        user_id: &str,
        include_deleted: bool,
        last_delete_at: i64,
        page_size: i64,
        from_channel_id: &str,
    ) -> Result<ChannelList, StoreError> {
        get_channels_by_user(
            &self.pool,
            user_id,
            include_deleted,
            last_delete_at,
            page_size,
            from_channel_id,
        )
        .await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, offset, limit, count))]
    async fn get_public_channels_for_team(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<ChannelList, StoreError> {
        get_public_channels_for_team(&self.pool, team_id, offset, limit).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, asked = channel_ids.len()))]
    async fn get_public_channels_by_ids_for_team(
        &self,
        team_id: &str,
        channel_ids: &[String],
    ) -> Result<ChannelList, StoreError> {
        get_public_channels_by_ids_for_team(&self.pool, team_id, channel_ids).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, offset, limit, count))]
    async fn get_private_channels_for_team(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<ChannelList, StoreError> {
        get_private_channels_for_team(&self.pool, team_id, offset, limit).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, offset, limit, count))]
    async fn get_deleted(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
        user_id: &str,
        skip_team_membership_check: bool,
    ) -> Result<ChannelList, StoreError> {
        get_deleted(
            &self.pool,
            team_id,
            offset,
            limit,
            user_id,
            skip_team_membership_check,
        )
        .await
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

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, asked = user_ids.len()))]
    async fn get_members_by_ids(
        &self,
        channel_id: &str,
        user_ids: &[String],
    ) -> Result<Vec<ChannelMember>, StoreError> {
        get_members_by_ids(&self.pool, channel_id, user_ids).await
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id))]
    async fn get_members_for_user(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> Result<Vec<ChannelMember>, StoreError> {
        get_members_for_user(&self.pool, team_id, user_id).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
    async fn get_member_last_viewed_at(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> Result<i64, StoreError> {
        get_member_last_viewed_at(&self.pool, channel_id, user_id).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, count))]
    async fn get_channel_members_timezones(
        &self,
        channel_id: &str,
    ) -> Result<Vec<StringMap>, StoreError> {
        get_channel_members_timezones(&self.pool, channel_id).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, count))]
    async fn get_pinned_posts(&self, channel_id: &str) -> Result<PostList, StoreError> {
        get_pinned_posts(&self.pool, channel_id).await
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

/// Port of `SqlChannelStore.GetForPost` (channel_store.go:3152) — the channel a post lives in.
///
/// The same column list as [`get`] (both build it from `channelSliceColumns`, channel_store.go:152)
/// joined through `Posts`, but **two predicates lighter, and the difference is the behaviour**:
///
/// - **No `Type IN ('O','P','D','G')` filter.** [`get`] carries `messageChannelTypes` and this
///   does not, so a post in a channel of some other type resolves here and would 404 there.
/// - **No `DeleteAt = 0` filter** on either table, so a post in an *archived* channel still
///   resolves. That is what makes `SessionHasPermissionToChannelByPost` answer at all for an
///   archived channel rather than falling through to its system-permission branch — archiving
///   makes a channel read-only, not invisible to the permission system.
///
/// Go wraps a missing row as an error rather than returning `nil, nil`, and every caller in
/// `authorization.go` treats that error as "fall through to the next branch" rather than as a
/// denial — see the by-post checks in `mm-app`.
#[tracing::instrument(skip(pool), fields(post_id = %post_id, found))]
pub async fn get_for_post(pool: &PgPool, post_id: &str) -> Result<Channel, StoreError> {
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
          INNER JOIN posts p ON c.id = p.channelid
         WHERE p.id = $1
        "#,
        post_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get Channel with postId={post_id}"),
        source,
    })?;

    let Some(row) = row else {
        tracing::Span::current().record("found", false);
        return Err(StoreError::NotFound {
            entity: "Channel",
            criteria: format!("postId={post_id}"),
        });
    };
    tracing::Span::current().record("found", true);

    channel_from_row(row)
}

/// Port of `SqlChannelStore.GetMemberForPost` (channel_store.go:2479).
///
/// [`get_member`] with the channel-id predicate replaced by a join through `Posts`: the caller
/// has a post and wants the asking user's membership of whatever channel it is in. The scheme
/// joins are identical, so the role resolution is identical — which matters, because this feeds
/// `roles_grant_permission` exactly as `get_member` does ([D-142]).
///
/// The `Channels` join is **INNER**, as in [`get_member`], so a post whose channel row is gone
/// yields nothing rather than a member with empty scheme defaults. Note this query does *not*
/// filter deleted channels either — see [`get_for_post`].
#[tracing::instrument(skip(pool), fields(post_id = %post_id, user_id = %user_id, found))]
pub async fn get_member_for_post(
    pool: &PgPool,
    post_id: &str,
    user_id: &str,
) -> Result<ChannelMember, StoreError> {
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
          INNER JOIN posts p ON cm.channelid = p.channelid
          INNER JOIN channels c ON cm.channelid = c.id
          LEFT JOIN schemes channelscheme ON c.schemeid = channelscheme.id
          LEFT JOIN teams t ON c.teamid = t.id
          LEFT JOIN schemes teamscheme ON t.schemeid = teamscheme.id
         WHERE cm.userid = $1
           AND p.id = $2
        "#,
        user_id,
        post_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get ChannelMember with postId={post_id} and userId={user_id}"),
        source,
    })?;

    let Some(row) = row else {
        tracing::Span::current().record("found", false);
        return Err(StoreError::NotFound {
            entity: "ChannelMember",
            criteria: format!("postId={post_id}, userId={user_id}"),
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

/// Port of `SqlChannelStore.GetMembersByIds` (channel_store.go:3992), the body of
/// `POST /api/v4/channels/{channel_id}/members/ids`.
///
/// The same `channelMembersForTeamWithSchemeSelectQuery` as [`get_members`], with the channel
/// predicate joined by a second one on `ChannelMembers.UserId`. What a reader would otherwise
/// get wrong is what is *absent*:
///
/// - **No `LIMIT`, no `OFFSET` and no `ORDER BY`** — the route is unpaginated and the row order
///   is the heap's, shared with Go through the shared table and promised by neither. The api4
///   caller's list is capped only by `SortedArrayFromJSON`'s de-duplication, so this is the one
///   ported member query a client can ask for a thousand rows from in one call.
/// - **No `DeleteAt` filter on anything** — not the channel's and not the user's, exactly as in
///   [`get_members`]. A deactivated user's membership and an archived channel's memberships are
///   both returned.
/// - **Unknown ids are silently absent rather than an error**, and an id list that matches
///   nothing is an **empty list, not a 404** — the opposite of
///   [`get_public_channels_by_ids_for_team`], whose Go twin raises `ErrNotFound` on zero rows.
///   Two sibling by-ids queries, opposite answers to the same shape; both pinned.
#[tracing::instrument(skip(pool, user_ids), fields(channel_id = %channel_id, asked = user_ids.len(), found))]
pub async fn get_members_by_ids(
    pool: &PgPool,
    channel_id: &str,
    user_ids: &[String],
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
           AND cm.userid = ANY($2::text[])
        "#,
        channel_id,
        user_ids
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to find ChannelMembers with channelId={channel_id} and userId in {user_ids:?}"
        ),
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

/// Port of `SqlChannelStore.GetMany` (channel_store.go:1043).
///
/// Column for column the same query as [`get`] — including both `AccessControlPolicies`
/// subqueries and the `Type IN (O, P, D, G)` filter that makes this "the *message* channels with
/// these ids" rather than "the channels" — with `Id = ANY(...)` in place of `Id = ?`.
///
/// **Zero rows is `ErrNotFound`, not an empty list** (channel_store.go:1062), and Go applies no
/// `ORDER BY`. Its only ported caller, `App::get_posts_by_ids`, builds an id-keyed map from the
/// result, so heap order is not wire surface there; do not assume that for the next caller.
///
/// `allowFromCache` is dropped, as everywhere else in this port — there is no cache layer to
/// consult, and a parameter no caller can act on is a lie at the call site.
/// Port of `SqlChannelStore.SearchInTeam` (channel_store.go:3598) — every **public** channel in
/// a team matching the term.
///
/// # The search reads `PublicChannels`, not `Channels`
///
/// Go selects the channel columns from `Channels` and joins `PublicChannels c` for everything
/// else: the team filter, the `ORDER BY`, and both halves of the search clause all read `c`.
/// That shadow table holds only public channels, so **a private channel is never a result** —
/// of this query or of [`search_for_user_in_team`], which joins it too. A port that searched
/// `Channels` directly would leak private channels into the browse dialog.
///
/// # `includeDeleted` is a constant `true` here
///
/// `App::search_channels` passes it literally (app/channel.go:3485), so the `DeleteAt = 0`
/// predicate Go would add is never added: **archived channels are results**. That is what the
/// browse dialog's "archived channels" tab reads.
///
/// # The clause is present or absent, never empty
///
/// The same rule [`autocomplete_in_team`] documents: `searchClause` returns nil when
/// [`sanitize_search_term`] yields the empty string, and a nil clause is *omitted*. `$2` is that
/// presence bit; when it is false both arms are short-circuited and the whole (ordered, limited)
/// list comes back.
pub async fn search_in_team(
    pool: &PgPool,
    team_id: &str,
    term: &str,
) -> Result<ChannelList, StoreError> {
    let sanitized = sanitize_search_term(term);
    let has_search = !sanitized.is_empty();
    let like_term = if has_search {
        wildcard_search_term(&sanitized)
    } else {
        String::new()
    };
    let fulltext_term = build_fulltext_term(term);
    let text_config = default_text_search_config(pool).await?;

    let rows = sqlx::query_as!(
        ChannelRow,
        r#"
        SELECT
                   ch.id AS "id!",
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
                   ch.autotranslation AS "autotranslation!",
                   ch.shared,
                   ch.totalmsgcountroot,
                   ch.lastrootpostat,
                   ch.bannerinfo,
                   ch.defaultcategoryname AS "defaultcategoryname!",
                   ch.discoverable AS "discoverable!",
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
           JOIN publicchannels c ON c.id = ch.id
          WHERE c.teamid = $1
            AND (NOT $2
                 OR LOWER(c.name) LIKE LOWER($3) ESCAPE '*'
                 OR LOWER(c.displayname) LIKE LOWER($3) ESCAPE '*'
                 OR LOWER(c.purpose) LIKE LOWER($3) ESCAPE '*'
                 OR to_tsvector($5::text::regconfig,
                                c.name || ' ' || c.displayname || ' ' || c.purpose)
                    @@ to_tsquery($5::text::regconfig, $4))
          ORDER BY c.displayname
          LIMIT 100
        "#,
        team_id,
        has_search,
        like_term,
        fulltext_term,
        text_config,
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to find Channels".to_owned(),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());
    let channels: Vec<_> = rows
        .into_iter()
        .map(channel_from_row)
        .collect::<Result<_, _>>()?;
    Ok(ChannelList(channels))
}

/// Port of `SqlChannelStore.SearchForUserInTeam` (channel_store.go:3620) — [`search_in_team`]
/// narrowed to the channels the caller is a member of.
///
/// The extra join is `ChannelMembers`, and it is on the **PublicChannels** id, so this is still
/// public channels only — a private channel the caller is in is not a result. See
/// [`search_in_team`] for the rest.
pub async fn search_for_user_in_team(
    pool: &PgPool,
    user_id: &str,
    team_id: &str,
    term: &str,
) -> Result<ChannelList, StoreError> {
    let sanitized = sanitize_search_term(term);
    let has_search = !sanitized.is_empty();
    let like_term = if has_search {
        wildcard_search_term(&sanitized)
    } else {
        String::new()
    };
    let fulltext_term = build_fulltext_term(term);
    let text_config = default_text_search_config(pool).await?;

    let rows = sqlx::query_as!(
        ChannelRow,
        r#"
        SELECT
                   ch.id AS "id!",
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
                   ch.autotranslation AS "autotranslation!",
                   ch.shared,
                   ch.totalmsgcountroot,
                   ch.lastrootpostat,
                   ch.bannerinfo,
                   ch.defaultcategoryname AS "defaultcategoryname!",
                   ch.discoverable AS "discoverable!",
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
           JOIN publicchannels c ON c.id = ch.id
           JOIN channelmembers cm ON cm.channelid = c.id
          WHERE c.teamid = $1
            AND cm.userid = $2
            AND (NOT $3
                 OR LOWER(c.name) LIKE LOWER($4) ESCAPE '*'
                 OR LOWER(c.displayname) LIKE LOWER($4) ESCAPE '*'
                 OR LOWER(c.purpose) LIKE LOWER($4) ESCAPE '*'
                 OR to_tsvector($6::text::regconfig,
                                c.name || ' ' || c.displayname || ' ' || c.purpose)
                    @@ to_tsquery($6::text::regconfig, $5))
          ORDER BY c.displayname
          LIMIT 100
        "#,
        team_id,
        user_id,
        has_search,
        like_term,
        fulltext_term,
        text_config,
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to find Channels".to_owned(),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());
    let channels: Vec<_> = rows
        .into_iter()
        .map(channel_from_row)
        .collect::<Result<_, _>>()?;
    Ok(ChannelList(channels))
}

#[tracing::instrument(skip(pool, ids), fields(asked = ids.len(), found))]
/// Port of `SqlChannelStore.GetChannelsMemberCount` (channel_store.go:2573).
///
/// # Every requested id gets a key, whether or not it has members
///
/// Go seeds a `defaults` map with `0` for each id and lets `scanRowsIntoMap` overwrite the ones
/// the query answered for. A channel with no live members is therefore `"<id>": 0` on the wire,
/// not an absent key — and the handler above only ever passes ids it has already resolved to
/// channels, so the zeros are real channels rather than typos.
///
/// # The join is the filter
///
/// `INNER JOIN Users … AND Users.DeleteAt = 0` — a **deactivated member is not counted**, and
/// there is no `ChannelMembers` deletion column to check because leaving a channel deletes the
/// row outright. Dropping the join, or its predicate, inflates every count by the deactivated
/// accounts that never left.
///
/// # `BTreeMap`, because the answer is a JSON object
///
/// `encoding/json` sorts map keys bytewise when it marshals ([D-027]), so the response object is
/// in ascending id order regardless of the request's. A `BTreeMap<String, _>` serialised straight
/// through reproduces that without a sort step.
pub async fn get_channels_member_count(
    pool: &PgPool,
    ids: &[String],
) -> Result<BTreeMap<String, i64>, StoreError> {
    let mut counts: BTreeMap<String, i64> = ids.iter().map(|id| (id.clone(), 0)).collect();

    if ids.is_empty() {
        return Ok(counts);
    }

    let rows = sqlx::query!(
        r#"
        SELECT cm.channelid    AS "channel_id!",
               COUNT(*)        AS "count!"
          FROM channelmembers cm
          INNER JOIN users u ON u.id = cm.userid
         WHERE cm.channelid = ANY($1::text[])
           AND u.deleteat = 0
         GROUP BY cm.channelid
        "#,
        ids
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to fetch member counts".to_owned(),
        source,
    })?;

    tracing::Span::current().record("counted", rows.len());
    for row in rows {
        counts.insert(row.channel_id, row.count);
    }
    Ok(counts)
}

pub async fn get_many(pool: &PgPool, ids: &[String]) -> Result<Vec<Channel>, StoreError> {
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
         WHERE c.id = ANY($1::text[])
           AND c.type IN ('O', 'P', 'D', 'G')
        "#,
        ids
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get channels with ids {ids:?}"),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());

    if rows.is_empty() {
        return Err(StoreError::NotFound {
            entity: "Channel",
            criteria: format!("ids={ids:?}"),
        });
    }

    rows.into_iter().map(channel_from_row).collect()
}

/// The characters `buildFulltextClause` (channel_store.go:3878) turns into spaces before
/// building a `tsquery`. Copied verbatim, including the order, because it is a membership test.
const SPACE_FULLTEXT_SEARCH_CHARS: &str = "<>+-()~:*\"!@&";

/// Postgres' `default_text_search_config`, which Go reads once at startup with
/// `SHOW default_text_search_config` (store.go:409) and then **interpolates into the SQL text**.
///
/// Passed as a `regconfig` parameter here instead of pasted into the statement: same operator,
/// same dictionary, and it keeps the query a single compile-checked literal. The value is read
/// from the same database at connect time, so a deployment that changes the setting changes both
/// servers together.
async fn default_text_search_config(pool: &PgPool) -> Result<String, StoreError> {
    let row: (String,) = sqlx::query_as("SHOW default_text_search_config")
        .fetch_one(pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to read default_text_search_config".to_owned(),
            source,
        })?;
    Ok(row.0)
}

/// Port of `sanitizeSearchTerm` (sqlstore/utils.go:62).
///
/// **Order matters and is Go's**: every occurrence of the escape character is *removed* first,
/// and only then are `%` and `_` escaped with it. So a term of `*` alone sanitises to the empty
/// string — which is what makes `?name=*` a request with **no search clause at all** rather than
/// a match-everything wildcard. Measured against the running server: it returns the same 50
/// channels as `?name=`.
fn sanitize_search_term(term: &str) -> String {
    let mut out = term.replace('*', "");
    for c in ['%', '_'] {
        out = out.replace(c, &format!("*{c}"));
    }
    out
}

/// Port of `wildcardSearchTerm` (team_store.go:88): `%term%`, lower-cased.
///
/// `go_to_lower` rather than `str::to_lowercase` — Go applies Unicode's *simple* mapping and
/// Rust's applies the full one, and they disagree on two characters. The SQL lowers both sides
/// again, so this only matters for the handful of runes where the two mappings differ.
fn wildcard_search_term(term: &str) -> String {
    mm_model::utils::go_to_lower(&format!("%{term}%"))
}

/// Port of `buildFulltextClause`'s term preparation (channel_store.go:3879).
///
/// Three steps, in Go's order: map [`SPACE_FULLTEXT_SEARCH_CHARS`] to spaces, drop every `|`,
/// then split on whitespace and rejoin with ` & `, suffixing each part with `:*` for prefix
/// matching. `strings.Fields` splits on Unicode whitespace and collapses runs, which is
/// `split_whitespace`.
///
/// An all-punctuation term reduces to the empty string. `to_tsquery(cfg, '')` is a **notice**,
/// not an error — checked against Postgres — so the clause stays in the query and simply matches
/// nothing.
fn build_fulltext_term(term: &str) -> String {
    let mapped: String = term
        .chars()
        .map(|c| {
            if SPACE_FULLTEXT_SEARCH_CHARS.contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    mapped
        .replace('|', "")
        .split_whitespace()
        .map(|part| format!("{part}:*"))
        .collect::<Vec<_>>()
        .join(" & ")
}

/// Port of `SqlChannelStore.AutocompleteInTeam` (channel_store.go:3443) — the Ctrl+K switcher.
///
/// # The search clause is present or absent, never empty
///
/// `searchClause` (channel_store.go:3919) returns **nil** when `buildLIKEClauseX` does, and that
/// happens exactly when [`sanitize_search_term`] yields the empty string — an empty term, or one
/// made only of `*`. A nil clause is not added to the query at all, so those requests return the
/// whole (limited, ordered) list rather than nothing. `$4` below is that presence bit; when it is
/// false the LIKE and the `tsquery` are both short-circuited away, matching Go's *omission* of
/// the clause rather than approximating it with an always-true predicate.
///
/// The full-text half is not decoration. `?name=town%20square` matches `town-square` on this
/// deployment through `to_tsquery`, and through nothing else: no single column contains the
/// string "town square", so every `LIKE` fails and only the concatenated `tsvector` matches.
///
/// # `includeDeleted` is always true, so archived channels are in the answer
///
/// `App.AutocompleteChannelsForTeam` hardcodes it (channel.go:3401), and the `DeleteAt = 0`
/// predicate is therefore never added. The switcher lists archived channels; that is Go's.
///
/// # Visibility
///
/// A guest sees only channels they are a member of. Everyone else sees every non-private channel,
/// plus the private ones they are a member of, plus private channels flagged `Discoverable`.
/// Go writes the non-guest arm as three disjuncts with redundant `Type = 'P'` guards on the last
/// two; they are dropped here because the first disjunct already covers every non-private row,
/// and keeping them would only add predicates no input can distinguish.
///
/// # Order is wire format
///
/// `CASE WHEN LOWER(DisplayName) LIKE … THEN 0 ELSE 1 END, DisplayName` — display-name matches
/// first, then alphabetical within each group, by the **database's** collation (both servers ask
/// the same Postgres, so they agree by construction). With no search term the `CASE` is dropped
/// in Go and short-circuits to 1 here, which is the same single-group ordering.
#[tracing::instrument(skip(pool), fields(team_id = %team_id, user_id = %user_id, is_guest, found))]
pub async fn autocomplete_in_team(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
    term: &str,
    is_guest: bool,
) -> Result<ChannelList, StoreError> {
    let sanitized = sanitize_search_term(term);
    let has_search = !sanitized.is_empty();
    let like_term = if has_search {
        wildcard_search_term(&sanitized)
    } else {
        String::new()
    };
    let fulltext_term = build_fulltext_term(term);
    let text_config = default_text_search_config(pool).await?;

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
         WHERE c.teamid = $1
           AND c.type IN ('O', 'P', 'D', 'G')
           AND (CASE
                    WHEN $3 THEN c.id IN (SELECT cm.channelid
                                            FROM channelmembers cm
                                           WHERE cm.userid = $2)
                    ELSE c.type <> 'P'
                         OR c.id IN (SELECT cm.channelid
                                       FROM channelmembers cm
                                      WHERE cm.userid = $2)
                         OR c.discoverable = TRUE
                END)
           AND (NOT $4
                OR LOWER(c.name) LIKE LOWER($5) ESCAPE '*'
                OR LOWER(c.displayname) LIKE LOWER($5) ESCAPE '*'
                OR LOWER(c.purpose) LIKE LOWER($5) ESCAPE '*'
                OR to_tsvector($7::text::regconfig, c.name || ' ' || c.displayname || ' ' || c.purpose)
                   @@ to_tsquery($7::text::regconfig, $6))
         ORDER BY CASE
                      WHEN $4 AND LOWER(c.displayname) LIKE LOWER($5) ESCAPE '*' THEN 0
                      ELSE 1
                  END,
                  c.displayname
         LIMIT 50
        "#,
        team_id,
        user_id,
        is_guest,
        has_search,
        like_term,
        fulltext_term,
        text_config,
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find Channels with term='{term}'"),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());

    Ok(ChannelList(
        rows.into_iter()
            .map(channel_from_row)
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

/// Port of `SqlChannelStore.AutocompleteInTeamForSearch` (channel_store.go:3464) — the search
/// box's channel suggestions, and the sibling of [`autocomplete_in_team`] that shares almost
/// nothing with it.
///
/// Four differences from that route, every one of them observable:
///
/// 1. **Membership is required for *every* channel**, public ones included, because the base
///    query `JOIN`s `ChannelMembers`. The switcher lists public channels you have never joined;
///    this lists only what is already in your sidebar.
/// 2. **Group messages come in from outside the team.** The team predicate is
///    `TeamId = ? OR (TeamId = '' AND Type = 'G')`, so a GM — which belongs to no team — answers
///    under every team's path. A DM does not reach the base query at all; it arrives through the
///    second pass below.
/// 3. **`LIKE` and full text are a `UNION`, not an `OR`.** Go builds the base query twice, adds
///    one clause to each, and unions the two with an outer `LIMIT 50` — with the comment that
///    the `OR` form produced a much worse plan. Reproduced literally: `UNION` also de-duplicates,
///    which an `OR` would not have needed to.
/// 4. **Both `LIMIT 50`s are inner, and there is a third pass.** Each side of the union is
///    limited, the union is limited again, and then up to 50 direct messages are **appended** —
///    so this route can answer **more than 50 channels**. Measured: 58 for an empty term.
///
/// See [`autocomplete_in_team_for_search_direct_messages`] for the display-name substitution,
/// which is the strangest thing here.
///
/// The `$5` flag is the same "search clause present or absent" bit [`autocomplete_in_team`] uses,
/// and for the same reason: an empty or all-`*` term makes `buildLIKEClauseX` return nil, and Go
/// then runs the **base query alone** — no union, no full text. With `$5` false the second
/// branch below contributes nothing and the `UNION` collapses to that base query.
#[tracing::instrument(skip(pool), fields(team_id = %team_id, user_id = %user_id, found))]
pub async fn autocomplete_in_team_for_search(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
    term: &str,
) -> Result<ChannelList, StoreError> {
    let sanitized = sanitize_search_term(term);
    let has_search = !sanitized.is_empty();
    let like_term = if has_search {
        wildcard_search_term(&sanitized)
    } else {
        String::new()
    };
    let fulltext_term = build_fulltext_term(term);
    let text_config = default_text_search_config(pool).await?;

    let rows = sqlx::query_as!(
        ChannelRow,
        r#"
        (SELECT
               c.id AS "id!",
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
               c.autotranslation AS "autotranslation!",
               c.shared,
               c.totalmsgcountroot,
               c.lastrootpostat,
               c.bannerinfo,
               c.defaultcategoryname AS "defaultcategoryname!",
               c.discoverable AS "discoverable!",
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
           JOIN channelmembers cm ON cm.channelid = c.id
          WHERE (c.teamid = $1 OR (c.teamid = '' AND c.type = 'G'))
            AND cm.userid = $2
            AND c.type IN ('O', 'P', 'D', 'G')
            AND (NOT $3
                 OR LOWER(c.name) LIKE LOWER($4) ESCAPE '*'
                 OR LOWER(c.displayname) LIKE LOWER($4) ESCAPE '*'
                 OR LOWER(c.purpose) LIKE LOWER($4) ESCAPE '*')
          LIMIT 50)
        UNION
        (SELECT
               c.id AS "id!",
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
               c.autotranslation AS "autotranslation!",
               c.shared,
               c.totalmsgcountroot,
               c.lastrootpostat,
               c.bannerinfo,
               c.defaultcategoryname AS "defaultcategoryname!",
               c.discoverable AS "discoverable!",
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
           JOIN channelmembers cm ON cm.channelid = c.id
          WHERE (c.teamid = $1 OR (c.teamid = '' AND c.type = 'G'))
            AND cm.userid = $2
            AND c.type IN ('O', 'P', 'D', 'G')
            AND $3
            AND to_tsvector($6::text::regconfig,
                            c.name || ' ' || c.displayname || ' ' || c.purpose)
                @@ to_tsquery($6::text::regconfig, $5)
          LIMIT 50)
        LIMIT 50
        "#,
        team_id,
        user_id,
        has_search,
        like_term,
        fulltext_term,
        text_config,
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find Channels with term='{term}'"),
        source,
    })?;

    let mut channels = rows
        .into_iter()
        .map(channel_from_row)
        .collect::<Result<Vec<_>, _>>()?;

    channels.extend(
        autocomplete_in_team_for_search_direct_messages(pool, user_id, has_search, &like_term)
            .await?,
    );

    // `sort.Slice(..., ToLower(a.DisplayName) < ToLower(b.DisplayName))` (channel_store.go:3543).
    //
    // **Go's sort is unstable and this one is not.** `sort.Slice` makes no promise about equal
    // keys, so two channels whose lower-cased display names are equal come back in an order Go
    // itself does not repeat — no port can match that, and a fixture with such a tie cannot be
    // byte-compared. `sort_by` keeps the union-then-direct-messages order for ties, which is at
    // least *an* order Go could have produced.
    //
    // `go_to_lower`, not `str::to_lowercase`: Go applies Unicode's simple mapping and Rust the
    // full one, and the two disagree on a handful of characters — which here would move a row.
    channels.sort_by(|a, b| {
        mm_model::utils::go_to_lower(&a.display_name)
            .cmp(&mm_model::utils::go_to_lower(&b.display_name))
    });

    tracing::Span::current().record("found", channels.len());
    Ok(ChannelList(channels))
}

/// Port of `SqlChannelStore.autocompleteInTeamForSearchDirectMessages` (channel_store.go:3550).
///
/// # It returns the *other user's username* in the display-name field
///
/// Go selects `channelSliceColumns(true, "C")` — which already contains `C.DisplayName` — and
/// then appends `OtherUsers.Username AS DisplayName`. Two output columns of the same name, and
/// `sqlx`'s scan takes the **last**, so the `Channel` handed back carries a display name the
/// `Channels` row does not have. That is deliberate: a direct message's stored display name is
/// empty, and the search box needs something to show. Reproduced by selecting the username into
/// that position rather than by relying on a duplicate-column rule.
///
/// # The other three things about it
///
/// - **No team predicate and no `DeleteAt` filter.** A direct message belongs to no team and is
///   listed whatever its state.
/// - **The search term is matched against the other user's `Username` and `Nickname`**, never
///   against the channel. So typing a colleague's name finds the DM, and typing the channel's
///   own name — a pair of user ids joined by `__` — finds nothing.
/// - **A self-DM is invisible.** The subquery requires `IU.Id <> userID`, and a channel whose
///   only member is the caller has no other user to join against, so the `INNER JOIN` drops it.
#[tracing::instrument(skip(pool), fields(user_id = %user_id, found))]
async fn autocomplete_in_team_for_search_direct_messages(
    pool: &PgPool,
    user_id: &str,
    has_search: bool,
    like_term: &str,
) -> Result<Vec<Channel>, StoreError> {
    let rows = sqlx::query_as!(
        ChannelRow,
        r#"
        SELECT
               c.id AS "id!",
               c.createat,
               c.updateat,
               c.deleteat,
               c.teamid,
               c.type::text AS "channel_type!",
               otherusers.username AS "displayname?",
               c.name,
               c.header,
               c.purpose,
               c.lastpostat,
               c.totalmsgcount,
               c.extraupdateat,
               c.creatorid,
               c.schemeid,
               c.groupconstrained,
               c.autotranslation AS "autotranslation!",
               c.shared,
               c.totalmsgcountroot,
               c.lastrootpostat,
               c.bannerinfo,
               c.defaultcategoryname AS "defaultcategoryname!",
               c.discoverable AS "discoverable!",
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
          JOIN channelmembers cm ON cm.channelid = c.id
         INNER JOIN (SELECT icm.channelid, iu.username
                       FROM users iu
                       JOIN channelmembers icm ON icm.userid = iu.id
                      WHERE iu.id <> $1
                        AND (NOT $2
                             OR LOWER(iu.username) LIKE LOWER($3) ESCAPE '*'
                             OR LOWER(iu.nickname) LIKE LOWER($3) ESCAPE '*')
                    ) AS otherusers ON otherusers.channelid = c.id
         WHERE c.type = 'D'
           AND cm.userid = $1
         LIMIT 50
        "#,
        user_id,
        has_search,
        like_term,
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to find direct-message Channels".to_owned(),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());

    rows.into_iter().map(channel_from_row).collect()
}

/// One row of `channelMembersWithSchemeSelectQuery` (channel_store.go:1776) — the same shape
/// [`ChannelMemberRow`] carries, plus the three team columns the `Teams` join adds.
///
/// A separate struct rather than an `Option`-tailed variant of the other because `query_as!`
/// binds columns positionally: the two queries genuinely have different result shapes, and
/// sharing a type would put the team columns on every member lookup in the file.
struct ChannelMemberWithTeamRow {
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
    teamdisplayname: String,
    teamname: String,
    teamupdateat: i64,
}

/// Port of `channelMemberWithTeamWithSchemeRoles.ToModel` (channel_store.go:376): the member
/// mapping every other lookup shares, with the three team fields laid beside it.
///
/// **The three `COALESCE`s are load-bearing.** `Teams` is a `LEFT JOIN`, and a direct or group
/// message has no team — so without them a DM's row would carry SQL NULLs where Go puts `""`,
/// `""` and `0`. Measured: the first membership the admin has is a DM, and it answers with all
/// three blank rather than absent.
fn channel_member_with_team_from_row(
    row: ChannelMemberWithTeamRow,
) -> Result<ChannelMemberWithTeamData, StoreError> {
    let (team_display_name, team_name, team_update_at) =
        (row.teamdisplayname, row.teamname, row.teamupdateat);
    let member = channel_member_from_row(ChannelMemberRow {
        channelid: row.channelid,
        userid: row.userid,
        roles: row.roles,
        lastviewedat: row.lastviewedat,
        msgcount: row.msgcount,
        mentioncount: row.mentioncount,
        mentioncountroot: row.mentioncountroot,
        urgentmentioncount: row.urgentmentioncount,
        msgcountroot: row.msgcountroot,
        notifyprops: row.notifyprops,
        lastupdateat: row.lastupdateat,
        schemeuser: row.schemeuser,
        schemeadmin: row.schemeadmin,
        schemeguest: row.schemeguest,
        teamschemedefaultguestrole: row.teamschemedefaultguestrole,
        teamschemedefaultuserrole: row.teamschemedefaultuserrole,
        teamschemedefaultadminrole: row.teamschemedefaultadminrole,
        channelschemedefaultguestrole: row.channelschemedefaultguestrole,
        channelschemedefaultuserrole: row.channelschemedefaultuserrole,
        channelschemedefaultadminrole: row.channelschemedefaultadminrole,
        autotranslationdisabled: row.autotranslationdisabled,
    })?;

    Ok(ChannelMemberWithTeamData {
        channel_member: member,
        team_display_name,
        team_name,
        team_update_at,
    })
}

/// Port of `SqlChannelStore.GetMembersForUserWithPagination` (channel_store.go:3285).
///
/// Every channel the user is a member of, **across every team**, ordered by channel id and cut
/// with `LIMIT`/`OFFSET`. Three things to know:
///
/// - **`Channels.Type NOT IN ('S')`** — `nonMessageBackingChannelTypes` (channel_store.go:52) is
///   spaces only, so this list is wider than the `messageChannelTypes` filter most of this file
///   uses: a board channel (`BO`/`BP`) *is* listed here and is not listed by `GetMany`.
/// - **`ORDER BY ChannelId ASC`**, which is what makes the cursor variant below able to walk it.
/// - **Zero rows is an empty list**, not `ErrNotFound` — unlike the cursor variant, whose miss is
///   how the streaming handler learns to stop.
#[tracing::instrument(skip(pool), fields(user_id = %user_id, page, per_page, found))]
pub async fn get_members_for_user_with_pagination(
    pool: &PgPool,
    user_id: &str,
    page: i64,
    per_page: i64,
) -> Result<ChannelMembersWithTeamData, StoreError> {
    let offset = page * per_page;

    let rows = sqlx::query_as!(
        ChannelMemberWithTeamRow,
        r#"
        SELECT
               cm.channelid,
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
               cm.autotranslationdisabled,
               COALESCE(t.displayname, '') AS "teamdisplayname!",
               COALESCE(t.name, '')        AS "teamname!",
               COALESCE(t.updateat, 0)     AS "teamupdateat!"
          FROM channelmembers cm
          INNER JOIN channels c ON cm.channelid = c.id
          LEFT JOIN schemes channelscheme ON c.schemeid = channelscheme.id
          LEFT JOIN teams t ON c.teamid = t.id
          LEFT JOIN schemes teamscheme ON t.schemeid = teamscheme.id
         WHERE cm.userid = $1
           AND c.type NOT IN ('S')
         ORDER BY cm.channelid ASC
         LIMIT $2 OFFSET $3
        "#,
        user_id,
        per_page,
        offset,
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find ChannelMembers data with and userId={user_id}"),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());

    rows.into_iter()
        .map(channel_member_with_team_from_row)
        .collect()
}

/// Port of `SqlChannelStore.GetMembersForUserWithCursorPagination` (channel_store.go:3299).
///
/// [`get_members_for_user_with_pagination`]'s query with `ChannelId > ?` in place of the offset —
/// and **an empty page is `ErrNotFound`**, not an empty list. That is not a quirk to iron out:
/// the api4 handler's streaming branch loops until it sees exactly that error, so a port
/// returning `[]` here would spin forever.
#[tracing::instrument(skip(pool), fields(user_id = %user_id, per_page, found))]
pub async fn get_members_for_user_with_cursor_pagination(
    pool: &PgPool,
    user_id: &str,
    per_page: i64,
    from_channel_id: &str,
) -> Result<ChannelMembersWithTeamData, StoreError> {
    let rows = sqlx::query_as!(
        ChannelMemberWithTeamRow,
        r#"
        SELECT
               cm.channelid,
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
               cm.autotranslationdisabled,
               COALESCE(t.displayname, '') AS "teamdisplayname!",
               COALESCE(t.name, '')        AS "teamname!",
               COALESCE(t.updateat, 0)     AS "teamupdateat!"
          FROM channelmembers cm
          INNER JOIN channels c ON cm.channelid = c.id
          LEFT JOIN schemes channelscheme ON c.schemeid = channelscheme.id
          LEFT JOIN teams t ON c.teamid = t.id
          LEFT JOIN schemes teamscheme ON t.schemeid = teamscheme.id
         WHERE cm.userid = $1
           AND cm.channelid > $2
           AND c.type NOT IN ('S')
         ORDER BY cm.channelid ASC
         LIMIT $3
        "#,
        user_id,
        from_channel_id,
        per_page,
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find ChannelMembers data with and userId={user_id}"),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());

    if rows.is_empty() {
        return Err(StoreError::NotFound {
            entity: "ChannelMembers",
            criteria: format!("userId={user_id}"),
        });
    }

    rows.into_iter()
        .map(channel_member_with_team_from_row)
        .collect()
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

/// Port of `SqlChannelStore.GetChannelsByUser` (channel_store.go:1264) — every message channel
/// the user is a member of, **in every team**, as one keyset page in `ORDER BY Channels.Id ASC`.
/// That order is on the wire: `getChannelsForUser` streams the pages back to back, so the
/// client sees the whole list in id order, not display-name order like its per-team sibling
/// [`get_channels`].
///
/// The differences from [`get_channels`], each one a branch a reader could plausibly carry over
/// by mistake:
///
/// - **No team predicate** at all — but a `LEFT JOIN Teams` so that a channel of an **archived
///   team** can be filtered. A DM or GM has `TeamId = ''`, matches no team row, and passes every
///   team test through the `Teams.Id IS NULL` arm.
/// - **Keyset, not offset:** `from_channel_id` non-empty adds `Channels.Id > ?`; `page_size`
///   of `-1` means no `LIMIT`. The handler passes 100 and the last id of the previous page.
/// - **Deletion filters apply to the team too.** Without `include_deleted`: `Channels.DeleteAt =
///   0 AND (Teams.DeleteAt = 0 OR Teams.Id IS NULL)`. With it and a non-zero `last_delete_at`:
///   a living-or-archived-since test on **both** the channel and the team. With it and zero:
///   no filter — archived channels of archived teams included.
/// - **`Type IN (O, P, D, G)`** — `messageChannelTypes`, as everywhere.
///
/// **Zero rows is `ErrNotFound`** (channel_store.go:1323), including for a page past the end —
/// which is the normal way the handler's loop ends when the total is a multiple of the page
/// size. The app layer turns it into `app.channel.get_channels.not_found.app_error`.
#[tracing::instrument(
    skip(pool),
    fields(user_id = %user_id, include_deleted, last_delete_at, page_size, from_channel_id = %from_channel_id)
)]
pub async fn get_channels_by_user(
    pool: &PgPool,
    user_id: &str,
    include_deleted: bool,
    last_delete_at: i64,
    page_size: i64,
    from_channel_id: &str,
) -> Result<ChannelList, StoreError> {
    // Go's `Limit(uint64(pageSize))` is skipped only for exactly -1; `LIMIT NULL` is "no limit"
    // in Postgres, so the one statement covers both.
    let limit: Option<i64> = (page_size != -1).then_some(page_size);
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
          LEFT JOIN teams t ON ch.teamid = t.id
         WHERE cm.userid = $1
           AND ch.type IN ('O', 'P', 'D', 'G')
           AND ($2::text = '' OR ch.id > $2)
           AND CASE
                 WHEN $3::boolean THEN (
                     $4::bigint = 0
                     OR ((ch.deleteat = 0 OR ch.deleteat >= $4)
                         AND (t.id IS NULL OR t.deleteat = 0 OR t.deleteat >= $4))
                 )
                 ELSE ch.deleteat = 0 AND (t.deleteat = 0 OR t.id IS NULL)
               END
         ORDER BY ch.id ASC
         LIMIT $5
        "#,
        user_id,
        from_channel_id,
        include_deleted,
        last_delete_at,
        limit
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get channels with UserId={user_id}"),
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

/// Port of `SqlChannelStore.GetPublicChannelsForTeam` (channel_store.go:1499) — the store behind
/// the webapp's "Browse channels" list.
///
/// Three things here are not what a reader would write from the route's name:
///
/// - **It does not filter on `Channels.Type`.** Membership of the `PublicChannels` table *is* the
///   type test: `upsertPublicChannelT` (channel_store.go:589) inserts a row only for
///   `ChannelTypeOpen` and **deletes** it for anything else, so a channel converted to private
///   loses its row. Adding `Channels.Type = 'O'` would pass every test against a coherent
///   database and hide the drift the join exists to expose.
/// - **Both the team and the deletion predicate are read off `pc`, not off `Channels`** — and so
///   is the `ORDER BY`. `PublicChannels` is a denormalised shadow of five channel columns; when
///   it disagrees with `Channels` the shadow wins here. Archiving *keeps* the row and copies the
///   new `DeleteAt` into it (measured), so `pc.DeleteAt = 0` is the archived-channel filter and
///   is load-bearing rather than redundant.
/// - **Offset paging, not the keyset [`get_channels_by_user`] uses.** `LIMIT 0` is a real limit,
///   so `per_page=0` is an empty page rather than "no limit" — the opposite of `GetMembers`,
///   whose `Limit > 0` guard turns the same zero into the whole channel.
///
/// **Zero rows is an empty list, not `ErrNotFound`.** Go declares `channels := model.ChannelList{}`
/// and `sqlx.Select` leaves it empty, so a page past the end is `200 []` — measured against the
/// running server, and the opposite of [`get_channels`]'s 404.
#[tracing::instrument(skip(pool), fields(team_id = %team_id, offset, limit))]
pub async fn get_public_channels_for_team(
    pool: &PgPool,
    team_id: &str,
    offset: i64,
    limit: i64,
) -> Result<ChannelList, StoreError> {
    let rows = sqlx::query_as!(
        ChannelRow,
        r#"
        SELECT channels.id,
               channels.createat,
               channels.updateat,
               channels.deleteat,
               channels.teamid,
               channels.type::text AS "channel_type!",
               channels.displayname,
               channels.name,
               channels.header,
               channels.purpose,
               channels.lastpostat,
               channels.totalmsgcount,
               channels.extraupdateat,
               channels.creatorid,
               channels.schemeid,
               channels.groupconstrained,
               channels.autotranslation,
               channels.shared,
               channels.totalmsgcountroot,
               channels.lastrootpostat,
               channels.bannerinfo,
               channels.defaultcategoryname,
               channels.discoverable,
               EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp
                    WHERE acp.id = channels.id AND acp.type = 'channel'
               ) AS "policy_enforced!",
               COALESCE((
                   SELECT acp.active FROM accesscontrolpolicies acp
                    WHERE acp.id = channels.id AND acp.type = 'channel' AND acp.active = TRUE
                    LIMIT 1
               ), false) AS "policy_is_active!"
          FROM channels
          JOIN publicchannels pc ON (pc.id = channels.id)
         WHERE pc.teamid = $1
           AND pc.deleteat = 0
         ORDER BY pc.displayname
         LIMIT $2 OFFSET $3
        "#,
        team_id,
        limit,
        offset
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find channel with teamId={team_id}"),
        source,
    })?;

    tracing::Span::current().record("count", rows.len());
    let channels = rows
        .into_iter()
        .map(channel_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChannelList(channels))
}

/// Port of `SqlChannelStore.GetPublicChannelsByIdsForTeam` (channel_store.go:1527), the body of
/// `POST /api/v4/teams/{team_id}/channels/ids`.
///
/// [`get_public_channels_for_team`] with the paging clause traded for an id list, and three
/// things a reader would plausibly write differently:
///
/// - **Zero rows is a `NotFound`, not an empty list.** Go's `len(data) == 0` check raises
///   `store.NewErrNotFound` and the app layer turns it into a **404**
///   (`app.channel.get_channels_by_ids.not_found.app_error`) — so asking for one id that is not
///   a public channel of this team is a 404 where the paginated sibling would have served `[]`.
///   Its by-ids twin [`get_members_by_ids`] does the opposite for the same shape.
/// - **Every predicate is on `PublicChannels`, not on `Channels`** — team, `DeleteAt` and even
///   the id. The shadow table holds a row only for a channel that is public and not archived, so
///   there is no `Type` predicate anywhere: a private channel's id simply matches nothing. A
///   port that filtered `Channels.DeleteAt` instead would still answer for an archived channel
///   whose shadow row Go deletes.
/// - **`ORDER BY pc.DisplayName`**, the shadow table's copy — kept in sync by the same triggers
///   that maintain the row, and the same column the paginated sibling orders by.
///
/// Go builds a `props` map and an `idQuery` string above the builder and then uses neither; the
/// live query is squirrel's `sq.Eq{"pc.Id": channelIds}`. Dead code, not a second code path.
#[tracing::instrument(skip(pool, channel_ids), fields(team_id = %team_id, asked = channel_ids.len(), found))]
pub async fn get_public_channels_by_ids_for_team(
    pool: &PgPool,
    team_id: &str,
    channel_ids: &[String],
) -> Result<ChannelList, StoreError> {
    let rows = sqlx::query_as!(
        ChannelRow,
        r#"
        SELECT channels.id,
               channels.createat,
               channels.updateat,
               channels.deleteat,
               channels.teamid,
               channels.type::text AS "channel_type!",
               channels.displayname,
               channels.name,
               channels.header,
               channels.purpose,
               channels.lastpostat,
               channels.totalmsgcount,
               channels.extraupdateat,
               channels.creatorid,
               channels.schemeid,
               channels.groupconstrained,
               channels.autotranslation,
               channels.shared,
               channels.totalmsgcountroot,
               channels.lastrootpostat,
               channels.bannerinfo,
               channels.defaultcategoryname,
               channels.discoverable,
               EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp
                    WHERE acp.id = channels.id AND acp.type = 'channel'
               ) AS "policy_enforced!",
               COALESCE((
                   SELECT acp.active FROM accesscontrolpolicies acp
                    WHERE acp.id = channels.id AND acp.type = 'channel' AND acp.active = TRUE
                    LIMIT 1
               ), false) AS "policy_is_active!"
          FROM channels
          JOIN publicchannels pc ON (pc.id = channels.id)
         WHERE pc.teamid = $1
           AND pc.deleteat = 0
           AND pc.id = ANY($2::text[])
         ORDER BY pc.displayname
        "#,
        team_id,
        channel_ids
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find Channels with teamId={team_id}"),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());

    if rows.is_empty() {
        return Err(StoreError::NotFound {
            entity: "Channel",
            criteria: format!("teamId={team_id}, channelIds={channel_ids:?}"),
        });
    }

    let channels = rows
        .into_iter()
        .map(channel_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChannelList(channels))
}

/// Port of `SqlChannelStore.GetPrivateChannelsForTeam` (channel_store.go:1476).
///
/// The public sibling's *shape*, one table shallower: no `PublicChannels` join, so the channel
/// type has to be written out — `Type = 'P'` exactly, **not** `messageChannelTypes` and not
/// `<> 'O'`, so a board or a space in the team is never listed. `TeamId` and `DeleteAt` are read
/// off `Channels` here because there is no shadow table to read them from, and the `ORDER BY` is
/// `Channels.DisplayName` for the same reason.
///
/// Same offset paging and same empty-list-not-404 as [`get_public_channels_for_team`].
#[tracing::instrument(skip(pool), fields(team_id = %team_id, offset, limit))]
pub async fn get_private_channels_for_team(
    pool: &PgPool,
    team_id: &str,
    offset: i64,
    limit: i64,
) -> Result<ChannelList, StoreError> {
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
         WHERE c.type = 'P'
           AND c.teamid = $1
           AND c.deleteat = 0
         ORDER BY c.displayname
         LIMIT $2 OFFSET $3
        "#,
        team_id,
        limit,
        offset
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find channel with teamId={team_id}"),
        source,
    })?;

    tracing::Span::current().record("count", rows.len());
    let channels = rows
        .into_iter()
        .map(channel_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ChannelList(channels))
}

/// Port of `SqlChannelStore.GetDeleted` (channel_store.go:1735) — the archived half of the browse
/// dialog.
///
/// Every predicate is the mirror image of a sibling's, which is what makes this one easy to get
/// wrong:
///
/// - **`DeleteAt <> 0`**, where both siblings say `= 0`. Archived is the whole point.
/// - **`TeamId = ? OR TeamId = ''`** — the teamless arm that makes a DM or GM appear under every
///   team, exactly as [`get_by_name`] does and unlike [`get_by_names`], which *omits* its
///   predicate instead. Together with the `skip` arm below this means a system admin's archived
///   DMs are listed under any team id they ask about.
/// - **`Type IN (O, P, D, G)`** — `messageChannelTypes`, so a board is not listed even when
///   archived.
/// - **The membership narrowing is skipped for `manage_system`, not for a team admin.** Without
///   the skip a caller sees *every* archived public channel of the team — no membership needed,
///   which is what "public" means — plus the archived private ones they still hold a
///   `ChannelMembers` row for, and **no** archived DMs or GMs at all, since neither arm admits
///   `D` or `G`. With the skip every type passes. Both halves measured against Go.
///
/// The predicate stays `Id IN (SELECT ChannelId …)` rather than becoming a join: a join would
/// duplicate a row for a member with two membership rows, which cannot happen, and would change
/// the plan for no reason.
///
/// Go's `sql.ErrNoRows` branch (channel_store.go:1772) maps to `store.NewErrNotFound`, but
/// `sqlx.Select` into a slice never returns that sentinel — zero rows is an empty slice — so the
/// 404 in `App.GetDeletedChannels` is unreachable on both servers. Reproduced as an empty list
/// and measured: a team with nothing archived answers `200 []`.
#[tracing::instrument(skip(pool), fields(team_id = %team_id, user_id = %user_id, offset, limit, skip_team_membership_check))]
pub async fn get_deleted(
    pool: &PgPool,
    team_id: &str,
    offset: i64,
    limit: i64,
    user_id: &str,
    skip_team_membership_check: bool,
) -> Result<ChannelList, StoreError> {
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
         WHERE (c.teamid = $1 OR c.teamid = '')
           AND c.deleteat <> 0
           AND c.type IN ('O', 'P', 'D', 'G')
           AND ($4::boolean
                OR c.type = 'O'
                OR (c.type = 'P'
                    AND c.id IN (SELECT cm.channelid FROM channelmembers cm WHERE cm.userid = $5)))
         ORDER BY c.displayname
         LIMIT $2 OFFSET $3
        "#,
        team_id,
        limit,
        offset,
        skip_team_membership_check,
        user_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to get deleted channels with TeamId={team_id} and UserId={user_id}"
        ),
        source,
    })?;

    tracing::Span::current().record("count", rows.len());
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

/// Port of `SqlChannelStore.GetMemberLastViewedAt` (channel_store.go:2462).
///
/// `COALESCE(LastViewedAt, 0)` is Go's, and the column really is nullable. The zero it produces
/// is **not** distinguishable from a member who has genuinely never viewed the channel, and
/// `GetPostsForChannelAroundLastUnread` treats both as "nothing is unread" and answers an empty
/// list — so a NULL here is a silently empty response rather than an error.
///
/// No row at all is a different thing entirely: `ErrNotFound`, which the app layer turns into a
/// **404** `api.channel.get_channel_member.missing.app_error`.
pub async fn get_member_last_viewed_at(
    pool: &PgPool,
    channel_id: &str,
    user_id: &str,
) -> Result<i64, StoreError> {
    sqlx::query_scalar!(
        r#"
        SELECT COALESCE(channelmembers.lastviewedat, 0) AS "last_viewed_at!"
          FROM channelmembers
         WHERE channelmembers.channelid = $1
           AND channelmembers.userid = $2
        "#,
        channel_id,
        user_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to get lastViewedAt with channelId={channel_id} and userId={user_id}"
        ),
        source,
    })?
    .ok_or_else(|| StoreError::NotFound {
        entity: "LastViewedAt",
        criteria: format!("channelId={channel_id}, userId={user_id}"),
    })
}

/// Port of `SqlChannelStore.GetChannelMembersTimezones` (channel_store.go:2214).
///
/// # Every filter belongs to the app layer, and there is no `ORDER BY`
///
/// One row per membership, in whatever order the scan yields, including rows whose timezone is
/// empty and rows that repeat a timezone another member already has. `App.GetChannelMembersTimezones`
/// drops the empty ones and deduplicates what is left — which is why this returning a bag rather
/// than a set is not sloppiness to tidy up here.
///
/// # The join is a `LEFT JOIN` and the column is nullable
///
/// A membership whose `Users` row has been hard-deleted contributes a NULL, and `StringMap.Scan`
/// leaves the map at its zero value for one. That row then fails the app layer's
/// empty-timezone test and is dropped, so it never reaches a client — but the query must not
/// turn it into an error on the way.
pub async fn get_channel_members_timezones(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Vec<StringMap>, StoreError> {
    let rows = sqlx::query_scalar!(
        r#"
        SELECT users.timezone AS "timezone?"
          FROM channelmembers
          LEFT JOIN users ON channelmembers.userid = users.id
         WHERE channelmembers.channelid = $1
        "#,
        channel_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to find user timezones for users in channels with channelId={channel_id}"
        ),
        source,
    })?;

    tracing::Span::current().record("count", rows.len());

    rows.into_iter()
        .map(|value| match value {
            // `StringMap.Scan` returns early on a NULL, leaving the zero value.
            None => Ok(StringMap::new()),
            Some(serde_json::Value::Object(map)) => map
                .into_iter()
                .map(|(key, value)| match value {
                    serde_json::Value::String(text) => Ok((key, text)),
                    other => Err(StoreError::Decode {
                        entity: "User",
                        column: "timezone",
                        source: serde::de::Error::custom(format!(
                            "timezone value for {key} is {other}, not a string"
                        )),
                    }),
                })
                .collect(),
            Some(other) => Err(StoreError::Decode {
                entity: "User",
                column: "timezone",
                source: serde::de::Error::custom(format!("timezone is {other}, not an object")),
            }),
        })
        .collect()
}

/// Port of `SqlChannelStore.GetPinnedPosts` (channel_store.go:959).
///
/// # Three predicates, and every one of them is fixed
///
/// `IsPinned = true`, `ChannelId = $1` and `DeleteAt = 0`. There is no `includeDeleted`
/// parameter and no caller that could supply one, so a pinned post that was later deleted is
/// gone from this list — which is what makes the pinned count on `getChannelStats`
/// ([`get_pinned_post_count`]) and this list agree.
///
/// # `ORDER BY CreateAt ASC` — oldest first, and it is the only list read that goes this way
///
/// Every other post list in this store is `CreateAt DESC`. The order reaches the client as
/// `order`, so flipping it is a wire change and not a detail; `getPostsForChannel` and this
/// route disagree on purpose.
///
/// # The `ReplyCount` subquery is unconditional here
///
/// `getRootPosts` computes it only when `skip_fetch_threads` is set; this query has no such
/// flag, so every pinned post reports its thread's live reply count. `DeleteAt = 0` inside the
/// subquery counts only undeleted replies, matching the `replyCountSubQuery` the post store
/// uses.
///
/// # Both maps, and `order` too
///
/// Go calls `AddPost` **and** `AddOrder` for each row, unlike `getParentsPosts`, so nothing
/// lands in `posts` without appearing in `order`. `NewPostList` has already materialised both
/// collections, which is why a channel with nothing pinned answers `{"order":[],"posts":{}}`
/// and not `null`s — and why this function does **not** call `MakeNonNil`, which Go does not
/// call either.
pub async fn get_pinned_posts(pool: &PgPool, channel_id: &str) -> Result<PostList, StoreError> {
    let rows = sqlx::query_as!(
        PostRow,
        r#"
        SELECT p.id,
               p.createat     AS "create_at!",
               p.updateat     AS "update_at!",
               p.editat       AS "edit_at!",
               p.deleteat     AS "delete_at!",
               p.ispinned     AS "is_pinned!",
               p.userid       AS "user_id!",
               p.channelid    AS "channel_id!",
               p.rootid       AS "root_id!",
               p.originalid   AS "original_id!",
               p.message      AS "message!",
               p.type         AS "post_type!",
               p.props        AS "props?",
               p.hashtags     AS "hashtags!",
               p.filenames    AS "filenames?",
               p.fileids      AS "file_ids?",
               p.hasreactions AS "has_reactions!",
               p.remoteid     AS "remote_id?",
               (SELECT COUNT(sub.id)
                  FROM posts sub
                 WHERE sub.rootid = (CASE WHEN p.rootid = '' THEN p.id ELSE p.rootid END)
                   AND sub.deleteat = 0) AS "reply_count!"
          FROM posts p
         WHERE p.ispinned = TRUE
           AND p.channelid = $1
           AND p.deleteat = 0
         ORDER BY p.createat ASC
        "#,
        channel_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to find Posts".to_owned(),
        source,
    })?;

    tracing::Span::current().record("count", rows.len());

    let mut list = PostList::new();
    for row in rows {
        let post = post_from_row(row)?;
        let id = post.id.clone();
        list.add_post(post);
        list.add_order(id);
    }
    Ok(list)
}

/// Port of `SqlChannelStore.GetChannelsByScheme` (channel_store.go:4071).
///
/// # Its type filter is **not** `Get`'s
///
/// `Get` uses `messageChannelTypes` — `IN ('O','P','D','G')` — and deliberately hides a board.
/// This query uses `nonMessageBackingChannelTypes`, which holds exactly one entry, `ChannelTypeSpace`
/// (`'S'`, channel_store.go:52), so it renders `Type NOT IN ('S')` and a **board is returned**.
/// The two constants sit thirteen lines apart in the same file and read alike; using either for
/// the other changes which channels a scheme reports owning.
///
/// `ORDER BY DisplayName` with no tiebreak, like every other scheme-scoped listing. Two channels
/// sharing a display name have no defined order in either server; reproduced rather than
/// stabilised, for the reason recorded on the audit query.
pub async fn get_channels_by_scheme(
    pool: &PgPool,
    scheme_id: &str,
    offset: i64,
    limit: i64,
) -> Result<Vec<Channel>, StoreError> {
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
         WHERE c.schemeid = $1
           AND c.type <> 'S'
         ORDER BY c.displayname
         LIMIT $2 OFFSET $3
        "#,
        scheme_id,
        limit,
        offset
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find Channels with schemeId={scheme_id}"),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());
    rows.into_iter().map(channel_from_row).collect()
}

/// The read-state a channel-view request needs, as the three "unreads and mentions" queries
/// return it (channel_store.go:2232, :2306, :2374).
///
/// Go returns a bare `([]string, []string, map[string]int64, error)`; naming the three makes the
/// call sites at `MarkChannelsAsViewed` (app/channel.go:3678) readable, because two of them are
/// id lists that differ only in *which* channels they hold.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UnreadsAndMentions {
    /// The channels with anything unread — the set `UpdateLastViewedAt` is then run over.
    pub with_unreads: Vec<String>,
    /// The subset whose notification level means a push notification was (or would have been)
    /// raised, so the clear goes out for them.
    pub with_mentions: Vec<String>,
    /// `max(Channels.LastPostAt, ChannelMembers.LastViewedAt)` per channel, for **every**
    /// membership the query matched — read or unread. This is what reaches the wire as
    /// `ChannelViewResponse.last_viewed_at_times`.
    ///
    /// A `BTreeMap` because Go's `encoding/json` sorts map keys when it marshals, so the ordering
    /// is wire surface and sorted is the ordering.
    pub read_times: BTreeMap<String, i64>,
}

/// One row of the three "unreads and mentions" queries, before classification.
struct UnreadRow {
    id: String,
    channel_type: String,
    total_msg_count: i64,
    last_post_at: i64,
    msg_count: i64,
    mention_count: i64,
    notify_props: Option<StringMap>,
    last_viewed_at: i64,
}

/// The classification body the three queries share **verbatim** (channel_store.go:2276-2299,
/// :2340-2363, :2408-2431). Ported once; the queries differ only in their `WHERE`.
///
/// Three decisions live here and each one is easy to invert:
///
/// - **A mention counts as unread on its own.** `hasUnreads` is `TotalMsgCount - MsgCount > 0`
///   *or* `hasMentions`, so a member whose `MsgCount` was written ahead of the channel's total —
///   which the schema permits — still gets marked read when they have a mention pending.
/// - **The channel's `push` prop falls back to the user's, and only when it is the literal
///   `"default"`.** A missing prop is `""`, which is neither `"default"` nor `"all"` nor
///   `"mention"`, so it falls through *all three* arms and the channel is never in
///   `with_mentions`. Substituting the user's props for a missing prop as well would send a push
///   clear for channels Go leaves alone.
/// - **A direct channel is treated as `all` regardless of its prop**, but a *group* channel is
///   not — `ChannelTypeGroup` is not in the test.
///
/// `user_notify_props` is `None` for a user whose column decoded to a nil map; Go's index of a
/// nil map is `""`, which the same fall-through covers.
fn classify_unreads_and_mentions(
    rows: Vec<UnreadRow>,
    user_notify_props: Option<&StringMap>,
) -> UnreadsAndMentions {
    let mut result = UnreadsAndMentions::default();

    for row in rows {
        let has_mentions = row.mention_count > 0;
        let has_unreads = (row.total_msg_count - row.msg_count) > 0 || has_mentions;

        if has_unreads {
            result.with_unreads.push(row.id.clone());
        }

        let channel_push = row
            .notify_props
            .as_ref()
            .and_then(|props| props.get(PUSH_NOTIFY_PROP))
            .map(String::as_str)
            .unwrap_or_default();
        let notify = if channel_push == CHANNEL_NOTIFY_DEFAULT {
            user_notify_props
                .and_then(|props| props.get(PUSH_NOTIFY_PROP))
                .map(String::as_str)
                .unwrap_or_default()
        } else {
            channel_push
        };

        if notify == USER_NOTIFY_ALL || row.channel_type == CHANNEL_TYPE_DIRECT {
            if has_unreads {
                result.with_mentions.push(row.id.clone());
            }
        } else if notify == USER_NOTIFY_MENTION && has_mentions {
            result.with_mentions.push(row.id.clone());
        }

        result
            .read_times
            .insert(row.id, row.last_post_at.max(row.last_viewed_at));
    }

    result
}

/// `ChannelMembers.NotifyProps` as [`get_member`] reads it: SQL `NULL` and the JSON value `null`
/// are different rows and Go turns both into a nil map rather than an error ([D-135]).
fn notify_props_from_column(
    entity: &'static str,
    value: Option<serde_json::Value>,
) -> Result<Option<StringMap>, StoreError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => Ok(Some(serde_json::from_value::<StringMap>(value).map_err(
            |source| StoreError::Decode {
                entity,
                column: "notifyprops",
                source,
            },
        )?)),
    }
}

/// Port of `SqlChannelStore.GetChannelsWithUnreadsAndWithMentions` (channel_store.go:2232).
///
/// **The deny-list is one type wide.** Go filters `Channels.Type NOT IN (S)` —
/// `nonMessageBackingChannelTypes` (channel_store.go:52) — and *not* the `IN (O, P, D, G)`
/// allow-list [`get`] uses. Boards (`BO`/`BP`) are therefore in scope here: a board channel the
/// caller is a member of is marked read by `POST /channels/members/{user_id}/view` even though
/// [`get`] would call the same channel missing. Narrowing this to the allow-list would silently
/// leave board read-state behind.
///
/// A channel id in the list the user is not a member of contributes nothing — the join is on
/// `ChannelMembers` — and is not an error.
#[tracing::instrument(skip(pool, user_notify_props), fields(user_id = %user_id, requested = channel_ids.len(), found))]
pub async fn get_channels_with_unreads_and_with_mentions(
    pool: &PgPool,
    channel_ids: &[String],
    user_id: &str,
    user_notify_props: Option<&StringMap>,
) -> Result<UnreadsAndMentions, StoreError> {
    let rows = sqlx::query!(
        r#"
        SELECT channels.id AS "id!",
               channels.type::text AS "channel_type!",
               channels.totalmsgcount AS "totalmsgcount!",
               channels.lastpostat AS "lastpostat!",
               channelmembers.msgcount AS "msgcount!",
               channelmembers.mentioncount AS "mentioncount!",
               channelmembers.notifyprops,
               channelmembers.lastviewedat AS "lastviewedat!"
          FROM channelmembers
         INNER JOIN channels ON channelmembers.channelid = channels.id
         WHERE channelmembers.channelid = ANY($1)
           AND channelmembers.userid = $2
           AND channels.type <> 'S'
        "#,
        channel_ids,
        user_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to find channels with unreads and with mentions data".to_owned(),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());

    let mut decoded = Vec::with_capacity(rows.len());
    for row in rows {
        decoded.push(UnreadRow {
            id: row.id,
            channel_type: row.channel_type,
            total_msg_count: row.totalmsgcount,
            last_post_at: row.lastpostat,
            msg_count: row.msgcount,
            mention_count: row.mentioncount,
            notify_props: notify_props_from_column("ChannelMember", row.notifyprops)?,
            last_viewed_at: row.lastviewedat,
        });
    }

    Ok(classify_unreads_and_mentions(decoded, user_notify_props))
}

/// Port of `SqlChannelStore.UpdateLastViewedAt` (channel_store.go:2838).
///
/// # It is one statement, and the rows it returns do not come from the UPDATE
///
/// Go builds `WITH c AS (SELECT … FROM Channels WHERE Id IN …), updated AS (UPDATE ChannelMembers
/// … FROM c …) SELECT Id, LastPostAt FROM c`. Two consequences a straightforward
/// `UPDATE … RETURNING` would get wrong:
///
/// - **The returned rows are the *channels*, not the memberships.** A channel id the user is not
///   a member of updates nothing and is still in the answer, with its `LastPostAt`.
/// - **Empty is only empty when no `Channels` row matched.** That — not "no membership was
///   updated" — is what raises `ErrInvalidInput`, which the app layer turns into a **400**.
///
/// # `LastUpdateAt` is set from `LastViewedAt`, not from now
///
/// Both are `greatest(cm.LastViewedAt, c.LastPostAt)`, and the `cm.LastViewedAt` inside them is
/// the value *before* this statement — Postgres evaluates every `SET` expression against the old
/// row — so the two columns end up equal. Writing `LastUpdateAt = now()` would drift a column the
/// client sorts on.
///
/// The map this returns is `LastPostAt` per channel, which is **not** what the view routes answer
/// with: `MarkChannelsAsViewed` discards it in favour of the `read_times` from
/// [`get_channels_with_unreads_and_with_mentions`] (app/channel.go:3705).
#[tracing::instrument(skip(pool), fields(user_id = %user_id, channels = channel_ids.len()))]
pub async fn update_last_viewed_at(
    pool: &PgPool,
    channel_ids: &[String],
    user_id: &str,
) -> Result<BTreeMap<String, i64>, StoreError> {
    if channel_ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    let rows = sqlx::query!(
        r#"
        WITH c AS (
            SELECT id, lastpostat, totalmsgcount, totalmsgcountroot
              FROM channels
             WHERE id = ANY($1)
        ),
        updated AS (
            UPDATE channelmembers cm
               SET mentioncount = 0,
                   mentioncountroot = 0,
                   urgentmentioncount = 0,
                   msgcount = greatest(cm.msgcount, c.totalmsgcount),
                   msgcountroot = greatest(cm.msgcountroot, c.totalmsgcountroot),
                   lastviewedat = greatest(cm.lastviewedat, c.lastpostat),
                   lastupdateat = greatest(cm.lastviewedat, c.lastpostat)
              FROM c
             WHERE cm.userid = $2 AND c.id = cm.channelid
        )
        SELECT id AS "id!", lastpostat AS "lastpostat!" FROM c
        "#,
        channel_ids,
        user_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to find ChannelMembers data with userId={user_id} and channelId in {}",
            go_slice_debug(channel_ids)
        ),
        source,
    })?;

    if rows.is_empty() {
        return Err(StoreError::InvalidInput {
            entity: "Channel",
            field: "Id",
            value: go_slice_debug(channel_ids),
        });
    }

    Ok(rows
        .into_iter()
        .map(|row| (row.id, row.lastpostat))
        .collect())
}

/// Go's `fmt.Sprintf("%v", []string{…})` — space-separated inside square brackets, no quoting.
/// Only ever reaches a log line or a store error's `Detail`, but reproducing it keeps the two
/// servers' logs comparable when a parity run diverges.
fn go_slice_debug(values: &[String]) -> String {
    format!("[{}]", values.join(" "))
}

/// Port of `SqlChannelStore.GetBoardChannel` (channel_store.go:1003).
///
/// Column for column [`get`], with `Type IN (BO, BP)` in place of the message-channel allow-list.
/// It exists for `rejectBoardChannelByID` (api4/channel.go:23), whose whole job is to answer
/// **400** for a board id on a `/channels` route rather than let it fall through to a 404 — so
/// "found" here is the *rejection* path, and a miss is the ordinary one.
#[tracing::instrument(skip(pool), fields(channel_id = %id, found))]
pub async fn get_board_channel(pool: &PgPool, id: &str) -> Result<Channel, StoreError> {
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
           AND c.type IN ('BO', 'BP')
        "#,
        id
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find board channel with id = {id}"),
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

    /// `sanitizeSearchTerm` removes the escape character **before** escaping `%` and `_`, so a
    /// term made only of `*` sanitises to nothing — and a nothing term means the search clause is
    /// omitted entirely rather than matching nothing. That is the difference between `?name=*`
    /// returning every channel (it does, measured) and returning none.
    #[test]
    fn sanitize_removes_stars_then_escapes_wildcards() {
        assert_eq!(sanitize_search_term(""), "");
        assert_eq!(sanitize_search_term("*"), "");
        assert_eq!(sanitize_search_term("***"), "");
        assert_eq!(sanitize_search_term("town"), "town");
        assert_eq!(sanitize_search_term("%"), "*%");
        assert_eq!(sanitize_search_term("_"), "*_");
        assert_eq!(sanitize_search_term("a%b_c"), "a*%b*_c");
        // The star is stripped first, so it never becomes an escape for the `%` beside it.
        assert_eq!(sanitize_search_term("*%"), "*%");
        // A space survives, so `?name=%20` *does* carry a search clause.
        assert_eq!(sanitize_search_term(" "), " ");
    }

    #[test]
    fn the_like_term_is_wrapped_and_lowered() {
        assert_eq!(wildcard_search_term("Town"), "%town%");
        assert_eq!(wildcard_search_term(""), "%%");
        assert_eq!(wildcard_search_term("*%"), "%*%%");
    }

    /// The classification branches the DB suite cannot separate, on the pure function.
    ///
    /// `db_channel_view_reads.rs` covers `all`, the `default` fall-back and the direct-channel
    /// override against real rows. What is left is the `mention` arm and the fall-through, and
    /// both are cheaper — and clearer — asserted here.
    ///
    /// **Transcribed from `channel_store.go:2276-2299`, not generated.** The classification lives
    /// inside a `SqlChannelStore` method that `reference/dump` cannot call without a database, so
    /// there is no fixture oracle for it; if upstream changes the branch order this test keeps
    /// passing while the port drifts. Recorded as such rather than dressed up.
    fn row(id: &str, channel_type: &str, mentions: i64, push: Option<&str>) -> UnreadRow {
        UnreadRow {
            id: id.to_owned(),
            channel_type: channel_type.to_owned(),
            total_msg_count: 40,
            last_post_at: 100,
            msg_count: 15,
            mention_count: mentions,
            notify_props: push.map(|value| {
                StringMap::from(BTreeMap::from([(
                    PUSH_NOTIFY_PROP.to_owned(),
                    value.to_owned(),
                )]))
            }),
            last_viewed_at: 0,
        }
    }

    #[test]
    fn the_mention_arm_needs_a_mention_and_not_merely_an_unread() {
        let rows = vec![
            row("unread", "O", 0, Some("mention")),
            row("mentioned", "O", 3, Some("mention")),
        ];
        let got = classify_unreads_and_mentions(rows, None);
        assert_eq!(
            got.with_unreads,
            vec!["unread".to_owned(), "mentioned".to_owned()],
            "both are unread — the second by its mention count alone"
        );
        assert_eq!(
            got.with_mentions,
            vec!["mentioned".to_owned()],
            "`mention` is not `all`"
        );
    }

    /// A membership with **no** `push` prop resolves to `""`, which is neither `default` (so the
    /// user's props are not consulted) nor `all` nor `mention` — so it falls through every arm
    /// and the channel is in no push clear at all. Substituting the user's props for a missing
    /// prop as well is the plausible wrong reading.
    #[test]
    fn a_missing_push_prop_falls_through_every_arm() {
        let user = StringMap::from(BTreeMap::from([(
            PUSH_NOTIFY_PROP.to_owned(),
            USER_NOTIFY_ALL.to_owned(),
        )]));
        let got = classify_unreads_and_mentions(vec![row("open", "O", 5, None)], Some(&user));
        assert_eq!(got.with_unreads, vec!["open".to_owned()]);
        assert!(
            got.with_mentions.is_empty(),
            "a missing prop is not `default`"
        );
    }

    /// A mention makes a channel unread even when the member's `MsgCount` is **ahead** of the
    /// channel's total, which the schema permits and the subtraction alone would call read.
    #[test]
    fn a_mention_alone_makes_a_channel_unread() {
        let mut r = row("ahead", "O", 2, Some("all"));
        r.msg_count = 90;
        let got = classify_unreads_and_mentions(vec![r], None);
        assert_eq!(got.with_unreads, vec!["ahead".to_owned()], "40 - 90 < 0");
        assert_eq!(got.with_mentions, vec!["ahead".to_owned()]);
    }

    /// `read_times` is the larger of the two columns, and it is populated for a channel that is
    /// in neither id list.
    #[test]
    fn read_times_covers_every_row_including_the_read_ones() {
        let mut caught_up = row("read", "O", 0, Some("all"));
        caught_up.msg_count = 40;
        caught_up.last_viewed_at = 900;
        let got = classify_unreads_and_mentions(vec![caught_up], None);
        assert!(got.with_unreads.is_empty());
        assert!(got.with_mentions.is_empty());
        assert_eq!(got.read_times.get("read"), Some(&900), "max(100, 900)");
    }

    /// `buildFulltextClause`'s term: punctuation to spaces, pipes dropped, each field suffixed
    /// with `:*` and joined by ` & `.
    #[test]
    fn the_fulltext_term_is_prefix_matched_and_anded() {
        assert_eq!(build_fulltext_term("town"), "town:*");
        assert_eq!(build_fulltext_term("town square"), "town:* & square:*");
        // Runs of whitespace collapse, like `strings.Fields`.
        assert_eq!(
            build_fulltext_term("  town   square  "),
            "town:* & square:*"
        );
        // Every character in the map becomes a separator, so a hyphenated name is two terms.
        assert_eq!(build_fulltext_term("town-square"), "town:* & square:*");
        assert_eq!(build_fulltext_term("a<b>c"), "a:* & b:* & c:*");
        // Pipes are deleted rather than spaced, so they join their neighbours.
        assert_eq!(build_fulltext_term("a|b"), "ab:*");
        // All-punctuation reduces to nothing; `to_tsquery(cfg, '')` is a notice, not an error.
        assert_eq!(build_fulltext_term("*&@"), "");
        assert_eq!(build_fulltext_term(""), "");
        // `%` and `_` are *not* in the map — they reach the tsquery as-is.
        assert_eq!(build_fulltext_term("%"), "%:*");
    }
}
