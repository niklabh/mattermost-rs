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

use mm_model::channel::{
    CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP, CHANNEL_TYPE_SPACE, Channel, ChannelBannerInfo,
    ChannelSearchOpts, ChannelWithTeamData,
};
use mm_model::channel_list::{ChannelList, ChannelListWithTeamData};
use mm_model::channel_member::{
    CHANNEL_MEMBER_NOTIFY_PROPS_MAX_RUNES, CHANNEL_NOTIFY_DEFAULT, ChannelMember,
    ChannelMemberWithTeamData, ChannelMembersWithTeamData, ChannelUnread, ChannelUnreadAt,
};
use mm_model::post::Post;
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

    /// Port of `SqlChannelStore.CountPostsAfter` (channel_store.go:2922) — **two** counts from
    /// one builder, the second with `RootId = ''` added.
    ///
    /// # `Gt`, not `Gte`
    ///
    /// "created after but not including the given timestamp", and every caller passes
    /// `post.CreateAt - 1` so that the post itself falls inside the window. An off-by-one here
    /// moves the new-messages line by exactly one post.
    ///
    /// # The type filter is a `NOT IN`, and it is the join/leave set, not the system set
    ///
    /// Ten types, the same ones `Post.IsJoinLeaveMessage` checks — so `system_header_change` and
    /// every other `system_*` post **is** counted. Reading the list as "system messages" and
    /// reaching for a `LIKE 'system_%'` would change the count on any channel whose header has
    /// been edited.
    ///
    /// `excluded_user_id` empty means "no filter"; non-empty adds `UserId <> ?`, which is how the
    /// DM branch of `countMentionsFromPost` counts *the other person's* posts as mentions while
    /// [`ChannelStore::update_last_viewed_at_post`] counts everybody's.
    fn count_posts_after(
        &self,
        channel_id: &str,
        timestamp: i64,
        excluded_user_id: &str,
    ) -> impl std::future::Future<Output = Result<(i64, i64), StoreError>> + Send;

    /// Port of `SqlChannelStore.CountUrgentPostsAfter` (channel_store.go:2896).
    ///
    /// Joins `PostsPriority` to `Posts` and counts the urgent ones in the same window
    /// [`ChannelStore::count_posts_after`] uses — but **without** the join/leave type filter,
    /// which is moot: a join/leave post cannot carry a priority row.
    fn count_urgent_posts_after(
        &self,
        channel_id: &str,
        timestamp: i64,
        excluded_user_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlChannelStore.UpdateLastViewedAtPost` (channel_store.go:2976) — the write
    /// behind `POST /api/v4/users/{user_id}/posts/{post_id}/set_unread`.
    ///
    /// # `MsgCount` is written as a subtraction against a live column
    ///
    /// `MsgCount = (SELECT TotalMsgCount FROM Channels WHERE Id = …) - unread`, where `unread` is
    /// what [`ChannelStore::count_posts_after`] just returned for the same window. Go's own
    /// comment says why it is not `SELECT count(*) FROM Posts`: on an old channel the total is
    /// large and the unread tail is small. The consequence is that the two reads are **not**
    /// atomic with the write — a post landing between them moves the result — which is Go's
    /// behaviour and not something to "fix" with a single statement.
    ///
    /// # `set_unread_count_root` zeroes the root count *before* the update, not after
    ///
    /// `if !setUnreadCountRoot { unreadRoot = 0 }`, so `MsgCountRoot` is written as
    /// `TotalMsgCountRoot - 0` — the member is marked **fully caught up on roots** while being
    /// marked unread on the channel. That is the CRT-unsupported reply branch of
    /// `markChannelAsUnreadFromPostCRTUnsupported`, and it is the whole reason the flag exists.
    /// Inverting it is invisible in any channel whose posts are all roots, because then
    /// `unreadRoot == unread` on one side and the caller passes `0` on the other only when they
    /// happen to coincide — see the behaviour fixture.
    ///
    /// # `LastViewedAt` is the post's `CreateAt - 1`, and `LastUpdateAt` is *now*
    ///
    /// Two different timestamps in the same `SET`, one derived from the post and one stamped. A
    /// reader who used `GetMillis()` for both would make every marked-unread channel look read.
    ///
    /// # The read-back is from the **master**, and a deleted channel returns no row
    ///
    /// `c.DeleteAt = 0` in the read-back, over a `LEFT JOIN` that the predicate turns into an
    /// inner one — so marking a post unread in a deleted channel performs the `UPDATE` and then
    /// fails on the `SELECT`. Reproduced: the write is not conditional on the channel being live.
    fn update_last_viewed_at_post(
        &self,
        unread_post: &Post,
        user_id: &str,
        mention_count: i64,
        mention_count_root: i64,
        urgent_mention_count: i64,
        set_unread_count_root: bool,
    ) -> impl std::future::Future<Output = Result<ChannelUnreadAt, StoreError>> + Send;

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

    /// Port of `SqlChannelStore.GetAllChannels` (channel_store.go:1341) — the system console's
    /// unfiltered list of every open and private channel, with its team's data beside it.
    fn get_all_channels(
        &self,
        offset: i64,
        limit: i64,
        opts: &ChannelSearchOpts,
    ) -> impl std::future::Future<Output = Result<ChannelListWithTeamData, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetAllChannelsCount` (channel_store.go:1363). **Not** the size of
    /// [`Self::get_all_channels`]: the count query omits the `Teams` join, so a channel with a
    /// dangling `TeamId` is counted and never listed.
    fn get_all_channels_count(
        &self,
        opts: &ChannelSearchOpts,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlChannelStore.SearchAllChannels` (channel_store.go:3778). The total is `0`
    /// unless `opts` carries **both** `page` and `per_page`; see the implementation.
    fn search_all_channels(
        &self,
        term: &str,
        opts: &ChannelSearchOpts,
    ) -> impl std::future::Future<Output = Result<(ChannelListWithTeamData, i64), StoreError>> + Send;

    /// Port of `SqlChannelStore.SearchGroupChannels` (channel_store.go:3977) — the caller's group
    /// messages, matched on the aggregated usernames of their members.
    fn search_group_channels(
        &self,
        user_id: &str,
        term: &str,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.AutocompleteInTeamFiltered` (channel_store.go:3447).
    fn autocomplete_in_team_filtered(
        &self,
        team_id: &str,
        user_id: &str,
        term: &str,
        is_guest: bool,
        private_only: bool,
        exclude_group_constrained: bool,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    /// Port of `SqlChannelStore.Autocomplete` (channel_store.go:3333) — every team the caller is
    /// still a member of, not one.
    fn autocomplete(
        &self,
        user_id: &str,
        term: &str,
        is_guest: bool,
    ) -> impl std::future::Future<Output = Result<ChannelListWithTeamData, StoreError>> + Send;

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

    /// Port of `SqlChannelStore.GetTeamChannelsWithUnreadAndMentions` (channel_store.go:2306).
    fn get_team_channels_with_unread_and_mentions(
        &self,
        team_id: &str,
        user_id: &str,
        user_notify_props: Option<&StringMap>,
    ) -> impl std::future::Future<Output = Result<UnreadsAndMentions, StoreError>> + Send;

    /// Port of `SqlChannelStore.GetDirectMessagesWithUnreadAndMentions` (channel_store.go:2374).
    fn get_direct_messages_with_unread_and_mentions(
        &self,
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

    // ---------------------------------------------------------------------------
    // Channel-member writes (`POST`/`PUT`/`DELETE …/channels/{id}/members…`)
    // ---------------------------------------------------------------------------

    /// Port of `SqlChannelStore.GetChannelOfType` (channel_store.go:1024).
    ///
    /// Unlike [`ChannelStore::get`] this carries **no** `Type IN ('O','P','D','G')` allow-list —
    /// it is how a caller reaches a backing channel type that `Get` deliberately hides, which is
    /// the entire reason `rejectSpaceChannelByID` (api4/channel.go:36) can answer 400 for a space
    /// id instead of letting it fall through to `Get`'s 404.
    fn get_channel_of_type(
        &self,
        id: &str,
        channel_type: &str,
    ) -> impl std::future::Future<Output = Result<Channel, StoreError>> + Send;

    /// Port of `SqlChannelStore.SaveMember` (channel_store.go:1827) and the
    /// `saveMultipleMembers` (channel_store.go:1835) behind it, for one member.
    ///
    /// Takes the member **by value** because Go's `PreSave` mutates it and the returned copy is a
    /// third value again: the row that was inserted, with its *effective* roles resolved.
    fn save_member(
        &self,
        member: ChannelMember,
    ) -> impl std::future::Future<Output = Result<ChannelMember, StoreError>> + Send;

    /// Port of `SqlChannelStore.UpdateMember` (channel_store.go:2052) and the
    /// `UpdateMultipleMembers` (channel_store.go:1991) behind it, for one member.
    fn update_member(
        &self,
        member: ChannelMember,
    ) -> impl std::future::Future<Output = Result<ChannelMember, StoreError>> + Send;

    /// Port of `SqlChannelStore.UpdateMemberNotifyProps` (channel_store.go:2060).
    ///
    /// A **merge**, not a replace: the SQL is `notifyprops || $1::jsonb`, so a key the caller did
    /// not send keeps its stored value.
    fn update_member_notify_props(
        &self,
        channel_id: &str,
        user_id: &str,
        props: &StringMap,
    ) -> impl std::future::Future<Output = Result<ChannelMember, StoreError>> + Send;

    /// Port of `SqlChannelStore.RemoveMember` (channel_store.go:2802), which is
    /// `RemoveMembers` (channel_store.go:2771) with a one-element list.
    /// Port of `SqlChannelStore.GetTeamSpaceChannelsForUser` (channel_store.go) — the space
    /// channels of one team that one user belongs to, in `Channels.Id` order.
    ///
    /// # Why a separate query exists at all
    ///
    /// `messageChannelTypes` is `('O','P','D','G')`, so [`get_channels`] — and every other
    /// membership listing — **cannot see a space channel**. Its `ChannelMembers` row therefore
    /// survives an ordinary team leave and keeps authorising space-scoped websocket delivery to a
    /// former member. That is the bug this function exists to prevent, and it is invisible in any
    /// test that only looks at the four message types.
    ///
    /// Zero rows is an empty list, **not** `ErrNotFound` — unlike [`get_channels`], whose empty
    /// result is an error. The caller does not special-case it.
    ///
    /// Spaces need `FeatureFlags.EnableDocs`, off on this deployment, so this returns nothing in
    /// practice today; the cascade is written because the flag is the only thing between here
    /// and a leaked membership.
    fn get_team_space_channels_for_user(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<ChannelList, StoreError>> + Send;

    fn remove_member(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlChannelStore.GetAllChannelMemberIdsByChannelId` (channel_store.go:1329).
    fn get_all_channel_member_ids_by_channel_id(
        &self,
        channel_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;
    // -----------------------------------------------------------------------------------------
    // Channel-row writes, behind the five channel-lifecycle routes (`PUT /channels/{id}`,
    // `/patch`, `/privacy`, `DELETE /channels/{id}`, `POST /channels/{id}/restore`).
    // Member writes are a separate group and live above; nothing here touches `ChannelMembers`.
    // -----------------------------------------------------------------------------------------

    /// Port of `SqlChannelStore.Update` (channel_store.go:845) and the `updateChannelT`
    /// (channel_store.go:868) inside its transaction.
    ///
    /// **The store mutates the channel it is handed**, exactly as Go does: `PreUpdate` mints a
    /// fresh `UpdateAt` and `SanitizeUnicode`s `Name`/`DisplayName` *before* validation, and the
    /// caller then publishes and returns that same value. Hence `&mut`; a by-value port would
    /// answer with the caller's stale `update_at`.
    ///
    /// Three error shapes the app layer tells apart, so they are three variants here:
    ///
    /// - `DeleteAt != 0` → [`StoreError::InvalidInput`]. **The guard is in the store, not the
    ///   handler**, which is why `PUT /channels/{id}/patch` on an archived channel answers
    ///   `app.channel.update.bad_id` while `PUT /channels/{id}` answers
    ///   `api.channel.update_channel.deleted.app_error` — measured against the running server.
    /// - `IsValid` → [`StoreError::Invalid`], carrying the model's own `AppError` so the id
    ///   (`model.channel.is_valid.*`) reaches the client unwrapped.
    /// - the `channels_name_teamid_key` unique constraint → [`StoreError::Conflict`] on `Name`.
    ///
    /// The write also propagates to `PublicChannels`, which is what makes a public↔private
    /// conversion visible to (or invisible in) every "public channels in this team" query.
    fn update(
        &self,
        channel: &mut Channel,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlChannelStore.Delete` (channel_store.go:1070) — a **soft** delete that writes
    /// `time` into both `DeleteAt` and `UpdateAt`.
    fn delete(
        &self,
        channel_id: &str,
        time: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlChannelStore.Restore` (channel_store.go:1075) — `DeleteAt = 0`, `UpdateAt =
    /// time`. The inverse of [`Self::delete`] and the same one query pair behind it.
    fn restore(
        &self,
        channel_id: &str,
        time: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlChannelStore.SetDeleteAt` (channel_store.go:1080).
    ///
    /// **No row-count check anywhere.** Setting `DeleteAt` on an id that does not exist updates
    /// nothing and returns `Ok` — the "is this channel really there / really archived" questions
    /// are all answered above the store, and a port that 404'd here would change which error the
    /// route reports.
    fn set_delete_at(
        &self,
        channel_id: &str,
        delete_at: i64,
        update_at: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    // -----------------------------------------------------------------------------------------
    // Channel creation (`POST /channels`, `/channels/direct`, `/channels/group`). Three entry
    // points onto one `saveChannelT`, and the differences between them are the whole group:
    // `Save` refuses `D` and boards and enforces the per-team limit, `save_direct_channel`
    // forces the type to `D` and writes both memberships in the same transaction, and a group
    // channel goes through `Save` with a hashed name and no team.
    // -----------------------------------------------------------------------------------------

    /// Port of `SqlChannelStore.Save` (channel_store.go:639).
    ///
    /// **Mutates the channel it is handed** — `PreSave` mints the id, the timestamps and the
    /// unicode-sanitised name — so a successful call leaves the caller holding the row that was
    /// written, which is exactly what the handler marshals.
    ///
    /// `max_channels_per_team` is `*TeamSettings.MaxChannelsPerTeam`; a **negative** value turns
    /// the limit off entirely (`maxChannelsPerTeam >= 0` guards the count), and `D`, `G` and `S`
    /// skip it regardless of the number.
    fn save(
        &self,
        channel: &mut Channel,
        max_channels_per_team: i64,
    ) -> impl std::future::Future<Output = Result<ChannelSave, StoreError>> + Send;

    /// Port of `SqlChannelStore.SaveDirectChannel` (channel_store.go:712).
    ///
    /// One transaction over three writes: the channel row and both memberships. **`team_id` is
    /// forced to the empty string** before the insert, so a DM is in no team and its uniqueness
    /// is `(name, '')` across the installation.
    ///
    /// When both members are the same user — Go allows a DM with yourself — only *one*
    /// `ChannelMembers` row is written (`saveMemberT(member2)`), not two. A port that wrote both
    /// would hit the primary key and turn a legal self-DM into a 500.
    fn save_direct_channel(
        &self,
        channel: &mut Channel,
        member1: ChannelMember,
        member2: ChannelMember,
    ) -> impl std::future::Future<Output = Result<ChannelSave, StoreError>> + Send;

    /// The size of `SqlChannelStore.GetTeamChannels` (channel_store.go:1571) without loading it.
    ///
    /// Go's `GetNumberOfChannelsOnTeam` fetches every channel of the team and takes `len`, so
    /// this counts the same set: `Type IN ('O','P','G')`, **archived channels included**, and
    /// no `DeleteAt` filter. It is deliberately *not* the count `save` enforces the limit with,
    /// which is a different predicate on the same table — see [`save`].
    ///
    /// **Zero is not an error here**; Go's list method answers `ErrNotFound` for an empty team
    /// and the app layer turns that into a 404, so that mapping lives in the app layer where the
    /// status code does.
    fn count_team_channels(
        &self,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;
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

    /// The `'urgent'` written into [`ChannelStore::count_urgent_posts_after`]'s SQL.
    ///
    /// `sqlx::query_scalar!` takes a string literal, so the value cannot be interpolated from
    /// [`mm_model::post::POST_PRIORITY_URGENT`]; this names it so a test can hold the two
    /// together.
    #[cfg(test)]
    const URGENT_PRIORITY_IN_SQL: &'static str = "urgent";
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

    async fn get_all_channels(
        &self,
        offset: i64,
        limit: i64,
        opts: &ChannelSearchOpts,
    ) -> Result<ChannelListWithTeamData, StoreError> {
        get_all_channels(&self.pool, offset, limit, opts).await
    }

    async fn get_all_channels_count(&self, opts: &ChannelSearchOpts) -> Result<i64, StoreError> {
        get_all_channels_count(&self.pool, opts).await
    }

    async fn search_all_channels(
        &self,
        term: &str,
        opts: &ChannelSearchOpts,
    ) -> Result<(ChannelListWithTeamData, i64), StoreError> {
        search_all_channels(&self.pool, term, opts).await
    }

    async fn search_group_channels(
        &self,
        user_id: &str,
        term: &str,
    ) -> Result<ChannelList, StoreError> {
        search_group_channels(&self.pool, user_id, term).await
    }

    async fn autocomplete_in_team_filtered(
        &self,
        team_id: &str,
        user_id: &str,
        term: &str,
        is_guest: bool,
        private_only: bool,
        exclude_group_constrained: bool,
    ) -> Result<ChannelList, StoreError> {
        autocomplete_in_team_filtered(
            &self.pool,
            team_id,
            user_id,
            term,
            is_guest,
            private_only,
            exclude_group_constrained,
        )
        .await
    }

    async fn autocomplete(
        &self,
        user_id: &str,
        term: &str,
        is_guest: bool,
    ) -> Result<ChannelListWithTeamData, StoreError> {
        autocomplete(&self.pool, user_id, term, is_guest).await
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

    async fn count_posts_after(
        &self,
        channel_id: &str,
        timestamp: i64,
        excluded_user_id: &str,
    ) -> Result<(i64, i64), StoreError> {
        count_posts_after(&self.pool, channel_id, timestamp, excluded_user_id).await
    }

    async fn count_urgent_posts_after(
        &self,
        channel_id: &str,
        timestamp: i64,
        excluded_user_id: &str,
    ) -> Result<i64, StoreError> {
        // `PostsPriority.Priority = 'urgent'` is [`mm_model::post::POST_PRIORITY_URGENT`], inline
        // rather than bound because it is a constant of the *query*, not of the request. Asserted
        // against the model constant in the test below, so a rename upstream fails a test rather
        // than silently counting nothing.
        //
        // **Nothing in the parity suite exercises this.** `POST /api/v4/posts` carrying a
        // `metadata.priority` is a 403 on the development stack, so no urgent post exists, the
        // count is always 0, and both arms of the `post_priority` config gate in
        // [`mm_app::App::count_mentions_from_post`] answer the same body. Measured, not assumed.
        let count = sqlx::query_scalar!(
            r#"
            SELECT count(*) AS "count!"
              FROM postspriority
              JOIN posts ON posts.id = postspriority.postid
             WHERE postspriority.priority = 'urgent'
               AND posts.channelid = $1
               AND posts.createat > $2
               AND posts.deleteat = 0
               AND ($3 = '' OR posts.userid <> $3)
            "#,
            channel_id,
            timestamp,
            excluded_user_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count urgent Posts".to_owned(),
            source,
        })?;
        Ok(count)
    }

    #[tracing::instrument(skip(self, unread_post), fields(channel_id = %unread_post.channel_id, user_id = %user_id, set_unread_count_root))]
    async fn update_last_viewed_at_post(
        &self,
        unread_post: &Post,
        user_id: &str,
        mention_count: i64,
        mention_count_root: i64,
        urgent_mention_count: i64,
        set_unread_count_root: bool,
    ) -> Result<ChannelUnreadAt, StoreError> {
        let unread_date = unread_post.create_at - 1;

        // The excluded user is the **empty string** here — every author's posts count towards
        // the channel's unread total, including the caller's own. `countMentionsFromPost` is the
        // call that excludes a user, and it is a different call.
        let (unread, unread_root) =
            count_posts_after(&self.pool, &unread_post.channel_id, unread_date, "").await?;

        let unread_root = if set_unread_count_root {
            unread_root
        } else {
            0
        };
        let updated_at = mm_model::utils::get_millis();

        sqlx::query!(
            r#"
            UPDATE channelmembers
               SET mentioncount       = $1,
                   mentioncountroot   = $2,
                   urgentmentioncount = $3,
                   msgcount     = (SELECT totalmsgcount     FROM channels WHERE id = $7) - $4,
                   msgcountroot = (SELECT totalmsgcountroot FROM channels WHERE id = $7) - $5,
                   lastviewedat = $6,
                   lastupdateat = $8
             WHERE userid = $9
               AND channelid = $7
            "#,
            mention_count,
            mention_count_root,
            urgent_mention_count,
            unread,
            unread_root,
            unread_date,
            unread_post.channel_id,
            updated_at,
            user_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to update ChannelMembers".to_owned(),
            source,
        })?;

        // `UrgentMentionCount` is the one coalesced column, exactly as in [`get_channel_unread`];
        // the rest scan into plain integers and a NULL there is a 500 on both servers.
        let row = sqlx::query!(
            r#"
            SELECT c.teamid                             AS "team_id!",
                   cm.userid                            AS "user_id!",
                   cm.channelid                         AS "channel_id!",
                   cm.msgcount                          AS "msg_count!",
                   cm.msgcountroot                      AS "msg_count_root!",
                   cm.mentioncount                      AS "mention_count!",
                   cm.mentioncountroot                  AS "mention_count_root!",
                   COALESCE(cm.urgentmentioncount, 0)   AS "urgent_mention_count!",
                   cm.lastviewedat                      AS "last_viewed_at!",
                   cm.notifyprops
              FROM channelmembers cm
              LEFT JOIN channels c ON c.id = cm.channelid
             WHERE cm.userid = $1
               AND cm.channelid = $2
               AND c.deleteat = 0
            "#,
            user_id,
            unread_post.channel_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to get ChannelMember with channelId={}",
                unread_post.channel_id
            ),
            source,
        })?;

        // `json:"-"`, so this never reaches a client — carried for the same reason
        // [`get_channel_unread`] carries it, and split on SQL-NULL-versus-JSON-null the same way.
        let notify_props =
            match row.notifyprops {
                None | Some(serde_json::Value::Null) => None,
                Some(value) => Some(serde_json::from_value::<StringMap>(value).map_err(
                    |source| StoreError::Decode {
                        entity: "ChannelUnreadAt",
                        column: "notifyprops",
                        source,
                    },
                )?),
            };

        Ok(ChannelUnreadAt {
            team_id: row.team_id,
            user_id: row.user_id,
            channel_id: row.channel_id,
            msg_count: row.msg_count,
            msg_count_root: row.msg_count_root,
            mention_count: row.mention_count,
            mention_count_root: row.mention_count_root,
            urgent_mention_count: row.urgent_mention_count,
            last_viewed_at: row.last_viewed_at,
            notify_props,
        })
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

    async fn get_team_channels_with_unread_and_mentions(
        &self,
        team_id: &str,
        user_id: &str,
        user_notify_props: Option<&StringMap>,
    ) -> Result<UnreadsAndMentions, StoreError> {
        get_team_channels_with_unread_and_mentions(&self.pool, team_id, user_id, user_notify_props)
            .await
    }

    async fn get_direct_messages_with_unread_and_mentions(
        &self,
        user_id: &str,
        user_notify_props: Option<&StringMap>,
    ) -> Result<UnreadsAndMentions, StoreError> {
        get_direct_messages_with_unread_and_mentions(&self.pool, user_id, user_notify_props).await
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

    // ---------------------------------------------------------------------------
    // Channel-member writes
    // ---------------------------------------------------------------------------

    #[tracing::instrument(skip_all, fields(channel_id = %id, channel_type = %channel_type))]
    async fn get_channel_of_type(
        &self,
        id: &str,
        channel_type: &str,
    ) -> Result<Channel, StoreError> {
        get_channel_of_type(&self.pool, id, channel_type).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %member.channel_id, user_id = %member.user_id))]
    async fn save_member(&self, member: ChannelMember) -> Result<ChannelMember, StoreError> {
        save_member(&self.pool, member).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %member.channel_id, user_id = %member.user_id))]
    async fn update_member(&self, member: ChannelMember) -> Result<ChannelMember, StoreError> {
        update_member(&self.pool, member).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id, keys = props.len()))]
    async fn update_member_notify_props(
        &self,
        channel_id: &str,
        user_id: &str,
        props: &StringMap,
    ) -> Result<ChannelMember, StoreError> {
        update_member_notify_props(&self.pool, channel_id, user_id, props).await
    }

    #[tracing::instrument(skip(self), fields(team_id = %team_id, user_id = %user_id, found))]
    async fn get_team_space_channels_for_user(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> Result<ChannelList, StoreError> {
        get_team_space_channels_for_user(&self.pool, team_id, user_id).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
    async fn remove_member(&self, channel_id: &str, user_id: &str) -> Result<(), StoreError> {
        remove_member(&self.pool, channel_id, user_id).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, members))]
    async fn get_all_channel_member_ids_by_channel_id(
        &self,
        channel_id: &str,
    ) -> Result<Vec<String>, StoreError> {
        get_all_channel_member_ids_by_channel_id(&self.pool, channel_id).await
    }
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id, channel_type = %channel.channel_type))]
    async fn update(&self, channel: &mut Channel) -> Result<(), StoreError> {
        update(&self.pool, channel).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, delete_at = time))]
    async fn delete(&self, channel_id: &str, time: i64) -> Result<(), StoreError> {
        // Go's one-liner: `Delete` is `SetDeleteAt(id, time, time)`, so an archived channel's
        // `UpdateAt` and `DeleteAt` are the same millisecond. Kept as a delegation rather than a
        // second statement so the two can never drift.
        set_delete_at(&self.pool, channel_id, time, time).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, update_at = time))]
    async fn restore(&self, channel_id: &str, time: i64) -> Result<(), StoreError> {
        set_delete_at(&self.pool, channel_id, 0, time).await
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, delete_at, update_at))]
    async fn set_delete_at(
        &self,
        channel_id: &str,
        delete_at: i64,
        update_at: i64,
    ) -> Result<(), StoreError> {
        set_delete_at(&self.pool, channel_id, delete_at, update_at).await
    }

    #[tracing::instrument(skip_all, fields(channel_type = %channel.channel_type, team_id = %channel.team_id))]
    async fn save(
        &self,
        channel: &mut Channel,
        max_channels_per_team: i64,
    ) -> Result<ChannelSave, StoreError> {
        save(&self.pool, channel, max_channels_per_team).await
    }

    #[tracing::instrument(skip_all, fields(name = %channel.name))]
    async fn save_direct_channel(
        &self,
        channel: &mut Channel,
        member1: ChannelMember,
        member2: ChannelMember,
    ) -> Result<ChannelSave, StoreError> {
        save_direct_channel(&self.pool, channel, member1, member2).await
    }

    #[tracing::instrument(skip(self), fields(team_id = %team_id))]
    async fn count_team_channels(&self, team_id: &str) -> Result<i64, StoreError> {
        count_team_channels(&self.pool, team_id).await
    }
}

/// What [`ChannelStore::save`] and [`ChannelStore::save_direct_channel`] did.
///
/// Go returns `(*Channel, error)` and its two values are *both* meaningful on the conflict path:
/// `saveChannelT` answers the **existing** channel alongside `ErrConflict("Channel")`, and three
/// callers read it — `CreateChannel` reports a 400 and throws the channel away, while
/// `GetOrCreateDirectChannel` and `CreateGroupChannel` swallow the error and return the channel
/// as a success. A bare `Result` cannot carry that, so the conflict is a value here rather than
/// an error.
///
/// `Existing` means **nothing was written**: the insert's `ON CONFLICT … DO NOTHING` matched no
/// row and the transaction is rolled back, so the memberships a direct channel would have
/// written are not there either.
#[derive(Debug)]
pub enum ChannelSave {
    /// The row was inserted. The caller's channel now holds what was written.
    Saved,
    /// `(Name, TeamId)` was taken. This is the row that already holds it — including, per Go's
    /// `tableSelectQuery`, an **archived** one, which is why re-creating a channel whose name is
    /// held by an archived channel is a conflict rather than a fresh insert.
    Existing(Box<Channel>),
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
    let row = select_member_with_scheme_roles(pool, channel_id, user_id)
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
pub async fn autocomplete_in_team(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
    term: &str,
    is_guest: bool,
) -> Result<ChannelList, StoreError> {
    autocomplete_in_team_query(pool, team_id, user_id, term, is_guest, false, false).await
}

/// Port of `SqlChannelStore.AutocompleteInTeamFiltered` (channel_store.go:3447) — the same query
/// with the two extra predicates `searchAllChannels`' non-console branch adds.
///
/// - **`private_only`** is `c.Type = 'P'` **and** `Shared` not true. The membership half is
///   already in the shared query, so this narrows rather than replaces it; the `Shared` clause is
///   there because Go's comment says a shared channel is ineligible for team-scoped access
///   control. `Shared` is nullable, so "not true" has to be spelled as two disjuncts.
/// - **`exclude_group_constrained`** drops the LDAP-group-synced channels, again with the
///   nullable column spelled out.
///
/// Go writes the second as `GroupConstrained = false OR GroupConstrained IS NULL`, not the
/// `<> true` its neighbour in [`search_all_channels`] uses; the two are the same truth table on a
/// boolean column and are kept apart only to match each call site.
#[tracing::instrument(skip(pool), fields(team_id = %team_id, user_id = %user_id, found))]
pub async fn autocomplete_in_team_filtered(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
    term: &str,
    is_guest: bool,
    private_only: bool,
    exclude_group_constrained: bool,
) -> Result<ChannelList, StoreError> {
    autocomplete_in_team_query(
        pool,
        team_id,
        user_id,
        term,
        is_guest,
        private_only,
        exclude_group_constrained,
    )
    .await
}

/// Port of `buildAutocompleteInTeamQuery` (channel_store.go:3405) plus the two filters
/// `AutocompleteInTeamFiltered` hangs off it — one query text, as Go has one builder.
#[tracing::instrument(skip(pool), fields(team_id = %team_id, user_id = %user_id, is_guest, found))]
async fn autocomplete_in_team_query(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
    term: &str,
    is_guest: bool,
    private_only: bool,
    exclude_group_constrained: bool,
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
           AND (NOT $8 OR (c.type = 'P' AND (c.shared IS NULL OR c.shared = false)))
           AND (NOT $9 OR (c.groupconstrained = false OR c.groupconstrained IS NULL))
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
        private_only,
        exclude_group_constrained,
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

/// Port of `SqlChannelStore.Autocomplete` (channel_store.go:3333) — [`autocomplete_in_team`]
/// without the team, and with the team's data returned beside each channel.
///
/// # The team is not a filter, it is a **membership join**
///
/// `FROM Channels c, Teams t, TeamMembers tm` with `c.TeamId = t.Id AND t.Id = tm.TeamId AND
/// tm.UserId = ?`: a channel is a candidate only if the caller is a member of its *team*. Two
/// consequences a reader could miss:
///
/// - **`tm.DeleteAt = 0` is applied unconditionally**, and Go's comment says why — a user removed
///   from a team must not see its channels "regardless of includeDeleted". The channel's own
///   `DeleteAt` is a separate, optional predicate; the membership's is not.
/// - **Direct and group messages are never rows.** They are in `messageChannelTypes`, so the type
///   filter admits them, but their `TeamId` is the empty string and the inner join to `Teams`
///   drops them. The switcher's cross-team list is teams-only for that reason alone.
///
/// Everything else — the guest split, the search clause, the display-name-match ordering and the
/// limit of 50 — is [`autocomplete_in_team`]'s, and that function documents it.
///
/// `include_deleted` is not a parameter here either: `App.AutocompleteChannels`
/// (app/channel.go:3379) hardcodes it to `true`, so the channel `DeleteAt` predicate is never
/// added and archived channels are listed.
#[tracing::instrument(skip(pool), fields(user_id = %user_id, is_guest, found))]
pub async fn autocomplete(
    pool: &PgPool,
    user_id: &str,
    term: &str,
    is_guest: bool,
) -> Result<ChannelListWithTeamData, StoreError> {
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
        ChannelWithTeamDataRow,
        r#"
        SELECT c.id AS "id!",
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
               ), false) AS "policy_is_active!",
               NULL::varchar AS "policyid",
               COALESCE(t.displayname, '') AS "teamdisplayname!",
               COALESCE(t.name, '') AS "teamname!",
               COALESCE(t.updateat, 0) AS "teamupdateat!"
          FROM channels c, teams t, teammembers tm
         WHERE c.teamid = t.id
           AND t.id = tm.teamid
           AND tm.userid = $1
           AND tm.deleteat = 0
           AND c.type IN ('O', 'P', 'D', 'G')
           AND (CASE
                    WHEN $2 THEN c.id IN (SELECT cm.channelid
                                            FROM channelmembers cm
                                           WHERE cm.userid = $1)
                    ELSE c.type <> 'P'
                         OR c.id IN (SELECT cm.channelid
                                       FROM channelmembers cm
                                      WHERE cm.userid = $1)
                         OR c.discoverable = TRUE
                END)
           AND (NOT $3
                OR LOWER(c.name) LIKE LOWER($4) ESCAPE '*'
                OR LOWER(c.displayname) LIKE LOWER($4) ESCAPE '*'
                OR LOWER(c.purpose) LIKE LOWER($4) ESCAPE '*'
                OR to_tsvector($6::text::regconfig,
                               c.name || ' ' || c.displayname || ' ' || c.purpose)
                   @@ to_tsquery($6::text::regconfig, $5))
         ORDER BY CASE
                      WHEN $3 AND LOWER(c.displayname) LIKE LOWER($4) ESCAPE '*' THEN 0
                      ELSE 1
                  END,
                  c.displayname
         LIMIT 50
        "#,
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
        context: format!("could not find channel with term={term}"),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());
    let channels: Vec<_> = rows
        .into_iter()
        .map(channel_with_team_data_from_row)
        .collect::<Result<_, _>>()?;
    Ok(ChannelListWithTeamData(channels))
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

/// Port of `SqlChannelStore.GetTeamSpaceChannelsForUser` — see the trait.
pub async fn get_team_space_channels_for_user(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
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
         WHERE ch.teamid = $1
           AND ch.type = 'S'
           AND cm.userid = $2
         ORDER BY ch.id
        "#,
        team_id,
        user_id,
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to find space Channels with teamId={team_id} and userId={user_id}"
        ),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());
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
#[cfg(test)]
mod urgent_priority_literal {
    /// The `'urgent'` literal in [`ChannelStore::count_urgent_posts_after`]'s SQL is
    /// `model.PostPriorityUrgent` (post.go:123). A `query_scalar!` cannot interpolate a constant,
    /// so the string is written out — and this is the only thing that notices if the model's value
    /// moves and the query keeps counting a priority that no longer exists.
    #[test]
    fn the_urgent_literal_is_the_model_constant() {
        assert_eq!(mm_model::post::POST_PRIORITY_URGENT, "urgent");
        assert!(
            super::SqlChannelStore::URGENT_PRIORITY_IN_SQL == mm_model::post::POST_PRIORITY_URGENT,
            "the SQL literal and the model constant must agree"
        );
    }
}

/// Port of `SqlChannelStore.CountPostsAfter` (channel_store.go:2922). A free function because
/// [`ChannelStore::update_last_viewed_at_post`] needs it on the same pool without going back
/// through the trait.
///
/// The two counts come from **one** builder in Go, the second adding `RootId = ''` — so every
/// other predicate is shared by construction. Spelled as two queries here; the `WHERE` clauses
/// must stay in step, which is what the behaviour fixture asserts.
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id, timestamp))]
pub async fn count_posts_after(
    pool: &PgPool,
    channel_id: &str,
    timestamp: i64,
    excluded_user_id: &str,
) -> Result<(i64, i64), StoreError> {
    // `Post.IsJoinLeaveMessage`'s ten types (post.go), as `NotEq` over a slice — Postgres
    // `NOT IN`. Every other `system_*` type is counted.
    const JOIN_LEAVE_TYPES: [&str; 10] = [
        mm_model::post::POST_TYPE_JOIN_LEAVE,
        mm_model::post::POST_TYPE_ADD_REMOVE,
        mm_model::post::POST_TYPE_JOIN_CHANNEL,
        mm_model::post::POST_TYPE_LEAVE_CHANNEL,
        mm_model::post::POST_TYPE_JOIN_TEAM,
        mm_model::post::POST_TYPE_LEAVE_TEAM,
        mm_model::post::POST_TYPE_ADD_TO_CHANNEL,
        mm_model::post::POST_TYPE_REMOVE_FROM_CHANNEL,
        mm_model::post::POST_TYPE_ADD_TO_TEAM,
        mm_model::post::POST_TYPE_REMOVE_FROM_TEAM,
    ];
    let excluded_types: Vec<String> = JOIN_LEAVE_TYPES.iter().map(|t| (*t).to_owned()).collect();

    let unread = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
          FROM posts
         WHERE channelid = $1
           AND createat > $2
           AND NOT (type = ANY($3))
           AND deleteat = 0
           AND ($4 = '' OR userid <> $4)
        "#,
        channel_id,
        timestamp,
        &excluded_types,
        excluded_user_id,
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to count Posts".to_owned(),
        source,
    })?;

    let unread_root = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
          FROM posts
         WHERE channelid = $1
           AND createat > $2
           AND NOT (type = ANY($3))
           AND deleteat = 0
           AND ($4 = '' OR userid <> $4)
           AND rootid = ''
        "#,
        channel_id,
        timestamp,
        &excluded_types,
        excluded_user_id,
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to count root Posts".to_owned(),
        source,
    })?;

    Ok((unread, unread_root))
}

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

/// One row of the three "unreads and mentions" queries, exactly as the SELECT returns it.
///
/// The field names are the **column** names because `query_as!` matches on them; the three
/// queries share this struct so that adding a column to one and not the others is a compile
/// error rather than a divergence.
struct UnreadAndMentionsRow {
    id: String,
    channel_type: String,
    totalmsgcount: i64,
    lastpostat: i64,
    msgcount: i64,
    mentioncount: i64,
    notifyprops: Option<serde_json::Value>,
    lastviewedat: i64,
}

/// The same row with `NotifyProps` decoded, which is the only per-row work the query does not do.
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

impl UnreadAndMentionsRow {
    fn decode(self) -> Result<UnreadRow, StoreError> {
        Ok(UnreadRow {
            id: self.id,
            channel_type: self.channel_type,
            total_msg_count: self.totalmsgcount,
            last_post_at: self.lastpostat,
            msg_count: self.msgcount,
            mention_count: self.mentioncount,
            notify_props: notify_props_from_column("ChannelMember", self.notifyprops)?,
            last_viewed_at: self.lastviewedat,
        })
    }
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
    let rows = sqlx::query_as!(
        UnreadAndMentionsRow,
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
    classify_rows(rows, user_notify_props)
}

/// Decode then classify — the tail every one of the three queries shares.
fn classify_rows(
    rows: Vec<UnreadAndMentionsRow>,
    user_notify_props: Option<&StringMap>,
) -> Result<UnreadsAndMentions, StoreError> {
    let decoded = rows
        .into_iter()
        .map(UnreadAndMentionsRow::decode)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(classify_unreads_and_mentions(decoded, user_notify_props))
}

/// Port of `SqlChannelStore.GetTeamChannelsWithUnreadAndMentions` (channel_store.go:2306).
///
/// [`get_channels_with_unreads_and_with_mentions`] scoped by **team** instead of by an id list,
/// and the difference is not only which rows come back: `readAllInTeam` passes the *whole*
/// result — including the channels that are already read — to the thread store, because a
/// CRT-enabled user can have unread thread replies in a channel whose channel-level counters are
/// up to date. Go's comment at app/channel.go:3566 says so.
///
/// The same one-type deny-list, so a board in the team is in scope.
#[tracing::instrument(skip(pool, user_notify_props), fields(team_id = %team_id, user_id = %user_id, found))]
pub async fn get_team_channels_with_unread_and_mentions(
    pool: &PgPool,
    team_id: &str,
    user_id: &str,
    user_notify_props: Option<&StringMap>,
) -> Result<UnreadsAndMentions, StoreError> {
    let rows = sqlx::query_as!(
        UnreadAndMentionsRow,
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
          JOIN channels ON channelmembers.channelid = channels.id
         WHERE channels.teamid = $1
           AND channelmembers.userid = $2
           AND channels.type <> 'S'
        "#,
        team_id,
        user_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to find team channels with unreads and mentions data".to_owned(),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());
    classify_rows(rows, user_notify_props)
}

/// Port of `SqlChannelStore.GetDirectMessagesWithUnreadAndMentions` (channel_store.go:2374).
///
/// **The one of the three with no deny-list**, because it has an allow-list instead: `Type IN
/// (D, G)`. Go writes the predicate against an unqualified `Type`, which resolves to
/// `Channels.Type` — `ChannelMembers` has no such column — and a direct channel carries no
/// `TeamId`, which is why this query is scoped by user alone.
#[tracing::instrument(skip(pool, user_notify_props), fields(user_id = %user_id, found))]
pub async fn get_direct_messages_with_unread_and_mentions(
    pool: &PgPool,
    user_id: &str,
    user_notify_props: Option<&StringMap>,
) -> Result<UnreadsAndMentions, StoreError> {
    let rows = sqlx::query_as!(
        UnreadAndMentionsRow,
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
          JOIN channels ON channelmembers.channelid = channels.id
         WHERE channelmembers.userid = $1
           AND channels.type IN ('D', 'G')
        "#,
        user_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to find direct or group channels with unreads and mentions data"
            .to_owned(),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());
    classify_rows(rows, user_notify_props)
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

/// Go's `channelMembersForTeamWithSchemeSelectQuery` (channel_store.go:558) with `GetMember`'s two
/// equality predicates, generic over the executor so the three member writes can re-select inside
/// their own transaction — which is where Go does it.
///
/// The join shape is Go's exactly:
///
///   - **INNER** on `Channels`. A membership row whose channel is gone returns *nothing*, not a
///     member with empty scheme defaults. Widening this to a LEFT join would resurrect orphaned
///     memberships, and a permission check reading one would grant against a channel that no
///     longer exists.
///   - **LEFT** on the two `Schemes` rows and on `Teams`. Every channel on Team Edition has a NULL
///     `SchemeId` — `Schemes` is an enterprise feature and the table is empty — so an INNER join
///     anywhere in that chain would return no members at all. `Teams` is LEFT because a DM or GM
///     channel has an empty `TeamId` and matches no team.
///
/// `COALESCE(UrgentMentionCount, 0)` is Go's, reproduced in SQL rather than defaulted Rust-side so
/// the database answers the same question for both servers.
async fn select_member_with_scheme_roles<'e, E>(
    executor: E,
    channel_id: &str,
    user_id: &str,
) -> Result<Option<ChannelMemberRow>, sqlx::Error>
where
    E: sqlx::PgExecutor<'e>,
{
    sqlx::query_as!(
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
    .fetch_optional(executor)
    .await
}

// ---------------------------------------------------------------------------
// Channel-member writes
//
// Five writes and one read behind `POST`, `PUT` and `DELETE` on
// `/api/v4/channels/{channel_id}/members…`. The read is `GetChannelOfType`, which those handlers
// need only so a space channel is refused with a 400 rather than a 404.
// ---------------------------------------------------------------------------

/// Port of `SqlChannelStore.GetChannelOfType` (channel_store.go:1024).
///
/// `tableSelectQuery` with `Id` and `Type` — the same column list as [`get`] and
/// [`get_board_channel`], and **no** message-channel allow-list. The type is compared as `text`
/// because `channels.type` is a Postgres enum and the caller names it with a `&str` constant.
#[tracing::instrument(skip(pool), fields(channel_id = %id, found))]
pub async fn get_channel_of_type(
    pool: &PgPool,
    id: &str,
    channel_type: &str,
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
         WHERE c.id = $1
           AND c.type::text = $2
        "#,
        id,
        channel_type
    )
    .fetch_optional(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find channel with id = {id} and type = {channel_type}"),
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

/// The two schemes' **channel**-role defaults for one channel, which is what a member's effective
/// roles fall back to. Go asks for these in two separate queries (channel_store.go:1863 and
/// :1899) keyed on the same channel id; one query with the same three LEFT JOINs answers both,
/// because `Channels.Id` and `Schemes.Id` are primary keys and neither join can fan out.
///
/// A channel id that matches nothing yields **no row**, which is Go's `map[...]` miss: every
/// default reads as `""` and `get_channel_roles` falls through to the constants. Reproduced rather
/// than turned into a not-found, because Go inserts the membership anyway.
struct SchemeDefaultsRow {
    channel_guest: Option<String>,
    channel_user: Option<String>,
    channel_admin: Option<String>,
    team_guest: Option<String>,
    team_user: Option<String>,
    team_admin: Option<String>,
}

/// Takes a `&mut PgConnection` rather than a `&PgPool` because
/// [`save_direct_channel`] runs it **inside** a transaction: the second member of a new DM is
/// saved against a channel row that is not committed yet, so a query on a pooled connection
/// would see no channel at all and silently resolve every role default to empty.
async fn scheme_defaults_for_channel(
    conn: &mut sqlx::PgConnection,
    channel_id: &str,
) -> Result<SchemeDefaultsRow, StoreError> {
    let row = sqlx::query_as!(
        SchemeDefaultsRow,
        r#"
        SELECT channelscheme.defaultchannelguestrole AS channel_guest,
               channelscheme.defaultchanneluserrole  AS channel_user,
               channelscheme.defaultchanneladminrole AS channel_admin,
               teamscheme.defaultchannelguestrole    AS team_guest,
               teamscheme.defaultchanneluserrole     AS team_user,
               teamscheme.defaultchanneladminrole    AS team_admin
          FROM channels c
          LEFT JOIN schemes channelscheme ON c.schemeid = channelscheme.id
          LEFT JOIN teams t ON c.teamid = t.id
          LEFT JOIN schemes teamscheme ON t.schemeid = teamscheme.id
         WHERE c.id = $1
        "#,
        channel_id
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("default_channel_roles_select channelId={channel_id}"),
        source,
    })?;

    Ok(row.unwrap_or(SchemeDefaultsRow {
        channel_guest: None,
        channel_user: None,
        channel_admin: None,
        team_guest: None,
        team_user: None,
        team_admin: None,
    }))
}

/// Go's `model.MapToJSON` reaching a `jsonb` parameter.
///
/// **A nil map is the JSON value `null`, not `{}`** — `json.Marshal` of a nil Go map writes
/// `null`, and the column is `jsonb`, so the row afterwards holds a JSON null rather than SQL
/// NULL or an empty object. [`channel_member_from_row`] reads all three back as `None`, so the
/// distinction is invisible on the wire and very visible in the table.
fn notify_props_to_jsonb(props: Option<&StringMap>) -> serde_json::Value {
    match props {
        None => serde_json::Value::Null,
        Some(props) => serde_json::json!(props),
    }
}

/// Port of `SqlChannelStore.SaveMember` (channel_store.go:1827) → `saveMultipleMembers`
/// (channel_store.go:1835), for the single-member case every ported caller uses.
///
/// # The three values here are not the same struct
///
/// 1. The **argument** as the app layer built it, with `explicit_roles` set and `roles` unused.
/// 2. The **row**, after `PreSave` stamps `last_update_at` — and note the `Roles` *column* is
///    written from `explicit_roles`, not from `roles` (`channelMemberToSlice`,
///    channel_store.go:226). Writing `roles` there would persist the scheme-implied role names as
///    explicit grants, which survives a scheme change and quietly outlives it.
/// 3. The **return**, whose `roles`, `explicit_roles` and three scheme flags come back out of
///    [`get_channel_roles`] — resolved against the schemes, not read back from the database. Go
///    never re-selects here.
///
/// # `IsValid` runs inside the store, so its error reaches the client unwrapped
///
/// A member with no `notify_props` fails `ChannelMember::is_valid` with
/// `model.channel_member.is_valid.notify_level.app_error` at 400, and the app layer's
/// `errors.As(err, &appErr)` passes it straight through. So the id a client sees for a malformed
/// member is a *model* id, not `app.channel.add_user.to.channel.failed.app_error`.
#[tracing::instrument(skip(pool, member), fields(channel_id = %member.channel_id, user_id = %member.user_id))]
pub async fn save_member(
    pool: &PgPool,
    member: ChannelMember,
) -> Result<ChannelMember, StoreError> {
    let mut conn = pool.acquire().await.map_err(|source| StoreError::Db {
        context: "channel_members_save: acquire".to_owned(),
        source,
    })?;
    save_member_on(&mut conn, member).await
}

/// [`save_member`] against a caller-owned connection, so a transaction can hold it.
///
/// `SaveDirectChannel` writes the channel row and both memberships in **one** transaction
/// (channel_store.go:712), and the scheme-defaults lookup inside reads the channel row the same
/// transaction has just inserted. Splitting the two across connections would make that read miss.
pub async fn save_member_on(
    conn: &mut sqlx::PgConnection,
    mut member: ChannelMember,
) -> Result<ChannelMember, StoreError> {
    let defaults = scheme_defaults_for_channel(&mut *conn, &member.channel_id).await?;

    member.pre_save();
    member.is_valid().map_err(|app_error| StoreError::Invalid {
        entity: "ChannelMember",
        app_error,
    })?;

    let notify_props = notify_props_to_jsonb(member.notify_props.as_ref());

    // `channelMemberSliceColumns()` (channel_store.go:146) in order. The order matters only for
    // readability against Go, but the *contents* matter: `roles` receives `explicit_roles`.
    let inserted = sqlx::query!(
        r#"
        INSERT INTO channelmembers
            (channelid, userid, roles, lastviewedat, msgcount, msgcountroot, mentioncount,
             mentioncountroot, urgentmentioncount, notifyprops, lastupdateat, schemeuser,
             schemeadmin, schemeguest, autotranslationdisabled)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        "#,
        member.channel_id,
        member.user_id,
        member.explicit_roles,
        member.last_viewed_at,
        member.msg_count,
        member.msg_count_root,
        member.mention_count,
        member.mention_count_root,
        member.urgent_mention_count,
        notify_props,
        member.last_update_at,
        member.scheme_user,
        member.scheme_admin,
        member.scheme_guest,
        member.auto_translation_disabled,
    )
    .execute(&mut *conn)
    .await;

    if let Err(source) = inserted {
        // Go's `IsUniqueConstraintError(execErr, []string{"ChannelId", "channelmembers_pkey", …})`
        // → `store.NewErrConflict("ChannelMembers", …)`. The app layer does not branch on it
        // (`AddUserToChannel` wraps everything into one 500), but the *resource* string is what a
        // future caller would branch on, so it is typed rather than folded into `Db`.
        if source
            .as_database_error()
            .is_some_and(|db| db.is_unique_violation())
        {
            return Err(StoreError::Conflict {
                resource: "ChannelMembers",
                source,
            });
        }
        return Err(StoreError::Db {
            context: format!(
                "channel_members_save channelId={}, userId={}",
                member.channel_id, member.user_id
            ),
            source,
        });
    }

    // `strings.Fields(member.ExplicitRoles)` — **not** `member.Roles`. Feeding `roles` in would
    // make every scheme-implied role an explicit one on the returned struct.
    let resolved = get_channel_roles(
        member.scheme_guest,
        member.scheme_user,
        member.scheme_admin,
        defaults.team_guest.as_deref().unwrap_or_default(),
        defaults.team_user.as_deref().unwrap_or_default(),
        defaults.team_admin.as_deref().unwrap_or_default(),
        defaults.channel_guest.as_deref().unwrap_or_default(),
        defaults.channel_user.as_deref().unwrap_or_default(),
        defaults.channel_admin.as_deref().unwrap_or_default(),
        &member.explicit_roles,
    );

    member.scheme_guest = resolved.scheme_guest;
    member.scheme_user = resolved.scheme_user;
    member.scheme_admin = resolved.scheme_admin;
    member.roles = resolved.roles.join(" ");
    member.explicit_roles = resolved.explicit_roles.join(" ");
    Ok(member)
}

/// Port of `SqlChannelStore.UpdateMember` (channel_store.go:2052) → `UpdateMultipleMembers`
/// (channel_store.go:1991), for the single-member case.
///
/// # It writes every column and then reads the row back
///
/// `NewMapFromChannelMemberModel` (channel_store.go:93) is a full `SET` map, not a patch — a
/// caller that loaded a member, changed one flag and passed it here also rewrites the unread
/// counters from whatever it happened to be holding. That is Go's contract and the reason
/// `updateChannelMemberNotifyProps` uses its own merge query instead of this.
///
/// The `Roles` column again receives `explicit_roles`, and the **return value comes from a fresh
/// `SELECT`** through the scheme joins — so the effective `roles` on the answer are the database's
/// view, not the caller's.
///
/// # The re-select is inside the transaction
///
/// Go's own comment wishes it were not ("TODO: Get this out of the transaction when is possible").
/// Kept inside, because moving it out changes what a concurrent write can interleave.
#[tracing::instrument(skip(pool, member), fields(channel_id = %member.channel_id, user_id = %member.user_id))]
pub async fn update_member(
    pool: &PgPool,
    mut member: ChannelMember,
) -> Result<ChannelMember, StoreError> {
    member.pre_update();
    member.is_valid().map_err(|app_error| StoreError::Invalid {
        entity: "ChannelMember",
        app_error,
    })?;

    let notify_props = notify_props_to_jsonb(member.notify_props.as_ref());

    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "begin_transaction".to_owned(),
        source,
    })?;

    sqlx::query!(
        r#"
        UPDATE channelmembers
           SET roles = $3,
               lastviewedat = $4,
               msgcount = $5,
               mentioncount = $6,
               mentioncountroot = $7,
               urgentmentioncount = $8,
               msgcountroot = $9,
               notifyprops = $10,
               lastupdateat = $11,
               schemeguest = $12,
               schemeuser = $13,
               schemeadmin = $14,
               autotranslationdisabled = $15
         WHERE channelid = $1
           AND userid = $2
        "#,
        member.channel_id,
        member.user_id,
        member.explicit_roles,
        member.last_viewed_at,
        member.msg_count,
        member.mention_count,
        member.mention_count_root,
        member.urgent_mention_count,
        member.msg_count_root,
        notify_props,
        member.last_update_at,
        member.scheme_guest,
        member.scheme_user,
        member.scheme_admin,
        member.auto_translation_disabled,
    )
    .execute(&mut *tx)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to update ChannelMember".to_owned(),
        source,
    })?;

    let row = select_member_with_scheme_roles(&mut *tx, &member.channel_id, &member.user_id)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to get ChannelMember with channelId={} and userId={}",
                member.channel_id, member.user_id
            ),
            source,
        })?;

    // **The UPDATE matching nothing is not the error** — the `SELECT` finding nothing is. Go's
    // `Exec` reports no rows affected and carries on; only `sql.ErrNoRows` from the read below
    // becomes `ErrNotFound`, which the app layer turns into a 404. Checking `rows_affected` here
    // instead would answer 404 for a no-op update of a member that does exist.
    let Some(row) = row else {
        return Err(StoreError::NotFound {
            entity: "ChannelMember",
            criteria: format!("channelId={}, userId={}", member.channel_id, member.user_id),
        });
    };

    let updated = channel_member_from_row(row)?;

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "commit_transaction".to_owned(),
        source,
    })?;

    Ok(updated)
}

/// Port of `SqlChannelStore.UpdateMemberNotifyProps` (channel_store.go:2060).
///
/// # `||` is a merge and the caller depends on it
///
/// `notifyprops = notifyprops || $1::jsonb` keeps every key the caller did not name. Replacing the
/// column instead would silently reset `mark_unread` — the muted flag — whenever a client saved
/// only `desktop`, which is what the webapp's notification dialog does.
///
/// # The rune cap is measured on Go's own encoding, before the query runs
///
/// `utf8.RuneCountInString(model.MapToJSON(props)) > model.ChannelMemberNotifyPropsMaxRunes`
/// → `store.NewErrInvalidInput`, which the app layer turns into a **400**
/// (`app.channel.update_member.notify_props_limit_exceeded.app_error`). Note it is checked on the
/// *submitted* props, not on the merged result, so a member already over the limit can be edited.
#[tracing::instrument(skip(pool, props), fields(channel_id = %channel_id, user_id = %user_id, keys = props.len()))]
pub async fn update_member_notify_props(
    pool: &PgPool,
    channel_id: &str,
    user_id: &str,
    props: &StringMap,
) -> Result<ChannelMember, StoreError> {
    let encoded = mm_model::utils::go_json_marshal_string_map(Some(props));
    if encoded.chars().count() > CHANNEL_MEMBER_NOTIFY_PROPS_MAX_RUNES {
        return Err(StoreError::InvalidInput {
            entity: "ChannelMember",
            field: "NotifyProps",
            value: format!("length={}", encoded.chars().count()),
        });
    }

    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "begin_transaction".to_owned(),
        source,
    })?;

    let patch = serde_json::json!(props);
    // `model.GetMillis()` is read at query-build time in Go, i.e. once per call.
    let now = mm_model::utils::get_millis();
    sqlx::query!(
        r#"
        UPDATE channelmembers
           SET notifyprops = notifyprops || $1::jsonb,
               lastupdateat = $2
         WHERE userid = $3
           AND channelid = $4
        "#,
        patch,
        now,
        user_id,
        channel_id
    )
    .execute(&mut *tx)
    .await
    .map_err(|source| StoreError::Db {
        context: format!(
            "failed to update ChannelMember with channelID={channel_id} and userID={user_id}"
        ),
        source,
    })?;

    let row = select_member_with_scheme_roles(&mut *tx, channel_id, user_id)
        .await
        .map_err(|source| StoreError::Db {
            context: format!(
                "failed to get ChannelMember with channelId={channel_id} and userId={user_id}"
            ),
            source,
        })?;

    let Some(row) = row else {
        return Err(StoreError::NotFound {
            entity: "ChannelMember",
            criteria: format!("channelId={channel_id}, userId={user_id}"),
        });
    };

    let updated = channel_member_from_row(row)?;

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "commit_transaction".to_owned(),
        source,
    })?;

    Ok(updated)
}

/// Port of `SqlChannelStore.RemoveMember` (channel_store.go:2802) → `RemoveMembers`
/// (channel_store.go:2771).
///
/// # Two deletes, no transaction, and the second one is not optional
///
/// The membership goes, and then the user's `SidebarChannels` rows for that channel go — "cleanup
/// sidebarchannels table if the user is no longer a member of that channel". Skipping it leaves a
/// channel pinned in the user's sidebar categories that they are no longer in, which
/// `GET /users/{id}/teams/{team}/channels/categories` will happily keep returning.
///
/// Go runs both on `GetMaster()` with **no transaction**, so a failure of the second leaves the
/// first committed. Reproduced: wrapping them would change which partial states are reachable.
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id, user_id = %user_id))]
pub async fn remove_member(
    pool: &PgPool,
    channel_id: &str,
    user_id: &str,
) -> Result<(), StoreError> {
    sqlx::query!(
        "DELETE FROM channelmembers WHERE channelid = $1 AND userid = $2",
        channel_id,
        user_id
    )
    .execute(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to delete ChannelMembers".to_owned(),
        source,
    })?;

    sqlx::query!(
        "DELETE FROM sidebarchannels WHERE channelid = $1 AND userid = $2",
        channel_id,
        user_id
    )
    .execute(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to delete SidebarChannels".to_owned(),
        source,
    })?;

    Ok(())
}
// ---------------------------------------------------------------------------------------------
// Channel-row writes
// ---------------------------------------------------------------------------------------------

/// `Channels.BannerInfo` as a `jsonb` parameter, reproducing
/// `(ChannelBannerInfo).Value` (channel.go:73).
///
/// **A pointer to an all-nil struct is stored as SQL NULL, not as `{}`.** Go's `Value()` compares
/// the struct against its zero value and returns `nil, nil` first, so `banner_info: {}` on the
/// wire and no `banner_info` at all reach the same column value. Serialising the empty struct
/// instead would put `{"enabled":null,"text":null,"background_color":null}` in the column, which
/// the read path would then hand back as `Some(default)` where Go hands back `None` — a
/// round-trip that changes the wire.
fn banner_info_column(
    banner: Option<&ChannelBannerInfo>,
) -> Result<Option<serde_json::Value>, StoreError> {
    let Some(banner) = banner else {
        return Ok(None);
    };
    if *banner == ChannelBannerInfo::default() {
        return Ok(None);
    }
    serde_json::to_value(banner)
        .map(Some)
        .map_err(|source| StoreError::Decode {
            entity: "Channel",
            column: "bannerinfo",
            source,
        })
}

/// Port of `IsUniqueConstraintError(err, []string{"Name", "channels_name_teamid_key"})` as
/// `updateChannelT` (channel_store.go:906) calls it.
///
/// Only this one constraint becomes a [`StoreError::Conflict`]. `PublicChannels` has a
/// `(Name, TeamId)` unique constraint of its own, and Go wraps a violation of *that* as an
/// ordinary "failed to insert public channel" error — a 500, not the 400 a duplicate name earns.
/// Widening the match would turn one into the other.
fn channel_name_conflict(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db| db.constraint())
        .is_some_and(|constraint| constraint == "channels_name_teamid_key")
}

/// Port of `SqlChannelStore.upsertPublicChannelT` (channel_store.go:589).
///
/// **A non-open channel is DELETEd from `PublicChannels` rather than upserted**, which is the
/// whole mechanism behind `PUT /channels/{id}/privacy`: converting to `P` takes the row out of
/// every public-channel listing and search, and converting back puts it in. Seven columns are
/// copied; `Type` is not among them, because membership of the table *is* the type.
async fn upsert_public_channel(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    channel: &Channel,
) -> Result<(), StoreError> {
    if channel.channel_type != mm_model::channel::CHANNEL_TYPE_OPEN {
        sqlx::query!("DELETE FROM publicchannels WHERE id = $1", channel.id)
            .execute(&mut **tx)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to delete public channel".to_owned(),
                source,
            })?;
        return Ok(());
    }

    sqlx::query!(
        r#"
        INSERT INTO publicchannels (id, deleteat, teamid, displayname, name, header, purpose)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (id) DO UPDATE
           SET deleteat    = $2,
               teamid      = $3,
               displayname = $4,
               name        = $5,
               header      = $6,
               purpose     = $7
        "#,
        channel.id,
        channel.delete_at,
        channel.team_id,
        channel.display_name,
        channel.name,
        channel.header,
        channel.purpose,
    )
    .execute(&mut **tx)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to insert public channel".to_owned(),
        source,
    })?;

    Ok(())
}

/// Port of `SqlChannelStore.GetAllChannelMemberIdsByChannelId` (channel_store.go:1329).
///
/// **Unordered and unpaginated** — Go's `SELECT UserId FROM ChannelMembers WHERE ChannelId=?` with
/// no `ORDER BY`, and `App.SetChannelMembers` immediately turns it into a set, so no caller may
/// depend on the order. An empty channel is an empty list, not a miss.
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id, members))]
pub async fn get_all_channel_member_ids_by_channel_id(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Vec<String>, StoreError> {
    let ids = sqlx::query_scalar!(
        r#"SELECT userid AS "userid!" FROM channelmembers WHERE channelid = $1"#,
        channel_id
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to get ChannelMembers with channelID={channel_id}"),
        source,
    })?;

    tracing::Span::current().record("members", ids.len());
    Ok(ids)
}
/// Port of `SqlChannelStore.Update` (channel_store.go:845) — see the trait for the error contract.
///
/// The order inside `updateChannelT` is load-bearing and each step is a plausible mutation:
/// `PreUpdate` **first** (so `UpdateAt` is fresh and the name is unicode-sanitised before it is
/// validated), then the `DeleteAt` guard, then `IsValid`, then the statement. Validating before
/// sanitising would accept a name Go rejects and vice versa.
///
/// Twenty-two columns are written, every one of them from the struct — including
/// `TotalMsgCount`, `LastPostAt` and `CreateAt`, which no route lets a client set but which are
/// nonetheless overwritten with whatever the caller read earlier. That is Go's lost-update
/// window and it is reproduced rather than narrowed: a port that wrote only the mutable columns
/// would keep counters Go clobbers.
#[tracing::instrument(skip(pool, channel), fields(channel_id = %channel.id))]
pub async fn update(pool: &PgPool, channel: &mut Channel) -> Result<(), StoreError> {
    channel.pre_update();

    if channel.delete_at != 0 {
        return Err(StoreError::InvalidInput {
            entity: "Channel",
            field: "DeleteAt",
            value: channel.delete_at.to_string(),
        });
    }

    channel
        .is_valid()
        .map_err(|app_error| StoreError::Invalid {
            entity: "Channel",
            app_error,
        })?;

    let banner_info = banner_info_column(channel.banner_info.as_ref())?;

    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "begin_transaction".to_owned(),
        source,
    })?;

    let affected = sqlx::query!(
        r#"
        UPDATE channels
           SET createat            = $2,
               updateat            = $3,
               deleteat            = $4,
               teamid              = $5,
               type                = $6::text::channel_type,
               displayname         = $7,
               name                = $8,
               header              = $9,
               purpose             = $10,
               lastpostat          = $11,
               totalmsgcount       = $12,
               extraupdateat       = $13,
               creatorid           = $14,
               schemeid            = $15,
               groupconstrained    = $16,
               shared              = $17,
               totalmsgcountroot   = $18,
               lastrootpostat      = $19,
               bannerinfo          = $20,
               defaultcategoryname = $21,
               autotranslation     = $22,
               discoverable        = $23
         WHERE id = $1
        "#,
        channel.id,
        channel.create_at,
        channel.update_at,
        channel.delete_at,
        channel.team_id,
        channel.channel_type,
        channel.display_name,
        channel.name,
        channel.header,
        channel.purpose,
        channel.last_post_at,
        channel.total_msg_count,
        channel.extra_update_at,
        channel.creator_id,
        channel.scheme_id,
        channel.group_constrained,
        channel.shared,
        channel.total_msg_count_root,
        channel.last_root_post_at,
        banner_info,
        channel.default_category_name,
        channel.auto_translation,
        channel.discoverable,
    )
    .execute(&mut *tx)
    .await
    .map_err(|source| {
        if channel_name_conflict(&source) {
            StoreError::Conflict {
                resource: "Name",
                source,
            }
        } else {
            StoreError::Db {
                context: format!("failed to update channel with id={}", channel.id),
                source,
            }
        }
    })?
    .rows_affected();

    // Go refuses `count > 1` and says nothing about zero: an `Id =` on the primary key can only
    // match one row, so this is a corruption assertion, and **zero rows is a success** — which is
    // how `PUT /channels/{id}` on a row deleted between the read and the write answers 200 with
    // a body nothing stored.
    if affected > 1 {
        return Err(StoreError::Db {
            context: format!(
                "the expected number of channels to be updated is <=1 but was {affected}"
            ),
            source: sqlx::Error::RowNotFound,
        });
    }

    upsert_public_channel(&mut tx, channel).await?;

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "commit_transaction".to_owned(),
        source,
    })
}

/// Port of `SqlChannelStore.SetDeleteAt` (channel_store.go:1080) and the `setDeleteAtT`
/// (channel_store.go:1119) inside its transaction.
///
/// Two statements, one transaction, and the **`PublicChannels` half only touches `DeleteAt`** —
/// unlike [`update`]'s upsert it neither inserts nor deletes, so a private channel simply has no
/// row here and the second statement is a no-op for it.
#[tracing::instrument(skip(pool), fields(channel_id = %channel_id, delete_at, update_at))]
pub async fn set_delete_at(
    pool: &PgPool,
    channel_id: &str,
    delete_at: i64,
    update_at: i64,
) -> Result<(), StoreError> {
    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "SetDeleteAt: begin_transaction".to_owned(),
        source,
    })?;

    sqlx::query!(
        "UPDATE channels SET deleteat = $1, updateat = $2 WHERE id = $3",
        delete_at,
        update_at,
        channel_id,
    )
    .execute(&mut *tx)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to delete channel with id={channel_id}"),
        source,
    })?;

    sqlx::query!(
        "UPDATE publicchannels SET deleteat = $1 WHERE id = $2",
        delete_at,
        channel_id,
    )
    .execute(&mut *tx)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to delete public channels with id={channel_id}"),
        source,
    })?;

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "SetDeleteAt: commit_transaction".to_owned(),
        source,
    })
}

// -------------------------------------------------------------------------------------------
// Channel creation
// -------------------------------------------------------------------------------------------

/// Port of `SqlChannelStore.Save` (channel_store.go:639) — see the trait for the contract.
///
/// # Three refusals before the transaction opens
///
/// `DeleteAt != 0`, `Type == 'D'` and a board type are all [`StoreError::InvalidInput`] on
/// `Channel`, and the app layer tells them apart by the **field** name: `DeleteAt` becomes
/// `store.sql_channel.save.archived_channel.app_error` and `Type` becomes
/// `store.sql_channel.save.direct_channel.app_error`. Folding the two into one variant would
/// swap one 400's id for another's.
///
/// A group channel is *not* refused here: `createGroupChannel` (app/channel.go:572) reaches this
/// same function with `Type == 'G'`, which is why the direct-channel guard names only `D`.
pub async fn save(
    pool: &PgPool,
    channel: &mut Channel,
    max_channels_per_team: i64,
) -> Result<ChannelSave, StoreError> {
    if channel.delete_at != 0 {
        return Err(StoreError::InvalidInput {
            entity: "Channel",
            field: "DeleteAt",
            value: channel.delete_at.to_string(),
        });
    }

    if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.is_board() {
        return Err(StoreError::InvalidInput {
            entity: "Channel",
            field: "Type",
            value: channel.channel_type.clone(),
        });
    }

    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "begin_transaction".to_owned(),
        source,
    })?;

    let saved = save_channel_t(&mut tx, channel, max_channels_per_team).await?;
    if let ChannelSave::Existing(existing) = saved {
        // Go's `return newChannel, err` leaves `finalizeTransactionX` to roll back. Nothing was
        // written, so the rollback is a formality — but it is the reason a conflicting create
        // cannot leave a `PublicChannels` row behind.
        drop(tx);
        return Ok(ChannelSave::Existing(existing));
    }

    upsert_public_channel(&mut tx, channel).await?;

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "commit_transaction".to_owned(),
        source,
    })?;

    Ok(ChannelSave::Saved)
}

/// Port of `SqlChannelStore.SaveDirectChannel` (channel_store.go:712) — see the trait.
///
/// **No `upsertPublicChannelT`.** A `D` channel is not public and Go does not call it here at
/// all, so unlike [`save`] there is no `PublicChannels` delete either; the row never existed.
pub async fn save_direct_channel(
    pool: &PgPool,
    channel: &mut Channel,
    mut member1: ChannelMember,
    mut member2: ChannelMember,
) -> Result<ChannelSave, StoreError> {
    if channel.delete_at != 0 {
        return Err(StoreError::InvalidInput {
            entity: "Channel",
            field: "DeleteAt",
            value: channel.delete_at.to_string(),
        });
    }

    if channel.channel_type != CHANNEL_TYPE_DIRECT {
        return Err(StoreError::InvalidInput {
            entity: "Channel",
            field: "Type",
            value: channel.channel_type.clone(),
        });
    }

    let mut tx = pool.begin().await.map_err(|source| StoreError::Db {
        context: "begin_transaction".to_owned(),
        source,
    })?;

    channel.team_id = String::new();
    let saved = save_channel_t(&mut tx, channel, 0).await?;
    if let ChannelSave::Existing(existing) = saved {
        drop(tx);
        return Ok(ChannelSave::Existing(existing));
    }

    // "Members need new channel ID" — `PreSave` minted it a moment ago.
    member1.channel_id.clone_from(&channel.id);
    member2.channel_id.clone_from(&channel.id);

    if member1.user_id != member2.user_id {
        save_member_on(&mut tx, member1).await?;
        save_member_on(&mut tx, member2).await?;
    } else {
        // A DM with yourself is one row, and Go saves **member2** — the `otherUser`. Both carry
        // the same user id here, so which one is chosen is invisible; it is kept literal
        // because the two differ in `scheme_guest`/`scheme_user` if a caller ever builds them
        // from two different users.
        save_member_on(&mut tx, member2).await?;
    }

    tx.commit().await.map_err(|source| StoreError::Db {
        context: "commit_transaction".to_owned(),
        source,
    })?;

    Ok(ChannelSave::Saved)
}

/// Port of `SqlChannelStore.saveChannelT` (channel_store.go:789).
///
/// # The order is the contract
///
/// The pre-existing-id guard, then `PreSave`, then `IsValid`, then the limit count, then the
/// insert. `PreSave` mints the id, so moving the first guard after it would reject every
/// channel; `PreSave` also unicode-sanitises the name, so validating first accepts names Go
/// rejects and vice versa.
///
/// # `Id != "" && !IsShared()` is not "the caller must not set an id"
///
/// A **shared** channel arrives with an id already assigned by the remote cluster and is
/// inserted with it. Every local create leaves it empty. So the guard is "a local caller may not
/// choose an id", and its error is `InvalidInput` on the `Id` field, which the app layer reports
/// as `store.sql_channel.save_channel.existing.app_error`.
///
/// # The limit count is a different query from `GetTeamChannels`
///
/// `DeleteAt = 0 AND (Type = 'O' OR Type = 'P')` — archived channels do **not** count against
/// the limit here, while [`count_team_channels`] (which the app layer's own pre-check uses)
/// counts them and counts `G` too. The two disagreeing is Go's behaviour, not a mistake.
async fn save_channel_t(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    channel: &mut Channel,
    max_channels_per_team: i64,
) -> Result<ChannelSave, StoreError> {
    if !channel.id.is_empty() && !channel.is_shared() {
        return Err(StoreError::InvalidInput {
            entity: "Channel",
            field: "Id",
            value: channel.id.clone(),
        });
    }

    channel.pre_save();
    channel
        .is_valid()
        .map_err(|app_error| StoreError::Invalid {
            entity: "Channel",
            app_error,
        })?;

    if channel.channel_type != CHANNEL_TYPE_DIRECT
        && channel.channel_type != CHANNEL_TYPE_GROUP
        && channel.channel_type != CHANNEL_TYPE_SPACE
        && max_channels_per_team >= 0
    {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(0) AS "count!"
              FROM channels
             WHERE teamid = $1
               AND deleteat = 0
               AND (type = 'O' OR type = 'P')
            "#,
            channel.team_id,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("save_channel_count: teamId={}", channel.team_id),
            source,
        })?;

        if count >= max_channels_per_team {
            return Err(StoreError::LimitExceeded {
                what: "channels_per_team",
                count,
                details: format!("teamId={}", channel.team_id),
            });
        }
    }

    let banner_info = banner_info_column(channel.banner_info.as_ref())?;

    // `channelSliceColumns(false)` / `channelToSlice` (channel_store.go:152, :198) in order.
    // `ON CONFLICT (name, teamid) DO NOTHING` is Go's `ON CONFLICT (TeamId, Name)` against the
    // `channels_name_teamid_key` index; Postgres infers by the column *set*, not their order.
    let inserted = sqlx::query!(
        r#"
        INSERT INTO channels
            (id, createat, updateat, deleteat, teamid, type, displayname, name, header, purpose,
             lastpostat, totalmsgcount, extraupdateat, creatorid, schemeid, groupconstrained,
             autotranslation, shared, totalmsgcountroot, lastrootpostat, bannerinfo,
             defaultcategoryname, discoverable)
        VALUES ($1, $2, $3, $4, $5, $6::text::channel_type, $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $16, $17, $18, $19, $20, $21, $22, $23)
        ON CONFLICT (name, teamid) DO NOTHING
        "#,
        channel.id,
        channel.create_at,
        channel.update_at,
        channel.delete_at,
        channel.team_id,
        channel.channel_type,
        channel.display_name,
        channel.name,
        channel.header,
        channel.purpose,
        channel.last_post_at,
        channel.total_msg_count,
        channel.extra_update_at,
        channel.creator_id,
        channel.scheme_id,
        channel.group_constrained,
        channel.auto_translation,
        channel.shared,
        channel.total_msg_count_root,
        channel.last_root_post_at,
        banner_info,
        channel.default_category_name,
        channel.discoverable,
    )
    .execute(&mut **tx)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("save_channel: id={}", channel.id),
        source,
    })?
    .rows_affected();

    if inserted != 0 {
        return Ok(ChannelSave::Saved);
    }

    // Go re-selects with `s.tableSelectQuery`, which carries **no** `Type IN (…)` filter and no
    // `DeleteAt` filter — so the row that took the name can be archived, or a type `Get` hides.
    // A failure to find it is *not* wrapped as a conflict on purpose (Go's own comment: "do not
    // return this as a *store.ErrConflict as it would be treated as a recoverable error"), so a
    // vanished duplicate is an ordinary 500 rather than a 400.
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
         WHERE c.teamid = $1
           AND c.name = $2
        "#,
        channel.team_id,
        channel.name,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("error while retrieving existing channel {}", channel.name),
        source,
    })?;

    Ok(ChannelSave::Existing(Box::new(channel_from_row(row)?)))
}

/// The size of `SqlChannelStore.GetTeamChannels` (channel_store.go:1571) — see the trait.
pub async fn count_team_channels(pool: &PgPool, team_id: &str) -> Result<i64, StoreError> {
    sqlx::query_scalar!(
        r#"
        SELECT COUNT(0) AS "count!"
          FROM channels
         WHERE teamid = $1
           AND type IN ('O', 'P', 'G')
        "#,
        team_id,
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find Channels with teamId={team_id}"),
        source,
    })
}

// ---------------------------------------------------------------------------
// Admin channel listing and search — `getAllChannels`, `searchAllChannels`,
// `searchGroupChannels` (api4/channel.go:1147, :1600, :739)
// ---------------------------------------------------------------------------

/// A [`ChannelRow`] with the three `Teams` columns and the optional retention `PolicyId` beside
/// it — the scan target for the two queries that build a `model.ChannelListWithTeamData`.
struct ChannelWithTeamDataRow {
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
    policyid: Option<String>,
    teamdisplayname: String,
    teamname: String,
    teamupdateat: i64,
}

/// Port of scanning `getAllChannelsQuery`'s row into a `model.ChannelWithTeamData`.
///
/// **`policy_id` is only ever set here.** Go adds the `RetentionPoliciesChannels.PolicyId`
/// column only when `IncludePolicyID` is on, so every other channel query leaves the field nil
/// and it serialises as `"policy_id":null` (no `omitempty`). The query below selects NULL for
/// that column when the flag is off, which reaches the same place.
fn channel_with_team_data_from_row(
    row: ChannelWithTeamDataRow,
) -> Result<ChannelWithTeamData, StoreError> {
    let (team_display_name, team_name, team_update_at, policy_id) = (
        row.teamdisplayname,
        row.teamname,
        row.teamupdateat,
        row.policyid,
    );
    let mut channel = channel_from_row(ChannelRow {
        id: row.id,
        createat: row.createat,
        updateat: row.updateat,
        deleteat: row.deleteat,
        teamid: row.teamid,
        channel_type: row.channel_type,
        displayname: row.displayname,
        name: row.name,
        header: row.header,
        purpose: row.purpose,
        lastpostat: row.lastpostat,
        totalmsgcount: row.totalmsgcount,
        extraupdateat: row.extraupdateat,
        creatorid: row.creatorid,
        schemeid: row.schemeid,
        groupconstrained: row.groupconstrained,
        autotranslation: row.autotranslation,
        shared: row.shared,
        totalmsgcountroot: row.totalmsgcountroot,
        lastrootpostat: row.lastrootpostat,
        bannerinfo: row.bannerinfo,
        defaultcategoryname: row.defaultcategoryname,
        discoverable: row.discoverable,
        policy_enforced: row.policy_enforced,
        policy_is_active: row.policy_is_active,
    })?;
    channel.policy_id = policy_id;

    Ok(ChannelWithTeamData {
        channel,
        team_display_name,
        team_name,
        team_update_at,
    })
}

/// Port of `SqlChannelStore.GetAllChannels` (channel_store.go:1341) and the
/// `getAllChannelsQuery` behind it (channel_store.go:1380) — the system console's channel list.
///
/// # This is not a search: there is no term and no visibility filter
///
/// Every open and private channel on the server, on every team, whether or not the caller is a
/// member. `c.Type IN ('P','O')` is the only type filter, so DMs, group messages, spaces and
/// board channels are never rows. The gate is entirely in the handler
/// (`getAllChannels`, api4/channel.go:1147), which demands one of three sysconsole permissions.
///
/// # `ORDER BY c.DisplayName, Teams.DisplayName` has no tie-break, and that is Go's
///
/// Two channels with the same display name on two teams with the same display name come back in
/// whatever order the plan produced. Adding `c.Id` would make this port *more* deterministic than
/// the server it must match, which is the wrong direction: a client that pages through the list
/// sees Go's order, ties and all. The parity fixture therefore gives every row a distinct display
/// name rather than relying on a tie-break that neither server has.
///
/// # The `Teams` join is in the list and **not** in the count
///
/// `getAllChannelsQuery` adds `JOIN Teams` only when `forCount` is false. A channel whose
/// `TeamId` names no `Teams` row is therefore counted by
/// [`get_all_channels_count`] and never listed — so `total_count` can exceed the number of
/// channels any amount of paging will yield. That is Go's arithmetic and it is reproduced, not
/// corrected.
///
/// # Which options this reaches
///
/// `getAllChannels` sets six of `ChannelSearchOpts`' fields and leaves the rest zero:
/// `not_associated_to_group`, `exclude_channel_names` (from `exclude_default_channels`),
/// `include_deleted`, `exclude_policy_constrained`, `access_control_policy_enforced` and
/// `exclude_access_control_policy_enforced`, plus `include_policy_id` from the caller's
/// retention-policy permission. `group_constrained`/`exclude_group_constrained` are in Go's
/// query builder but no path into this route sets them, so they are carried here too — the
/// predicate is shared with [`search_all_channels`], where the request body *can* set them, and
/// leaving it out of one of the two would be a difference a reader could not see.
#[tracing::instrument(skip(pool, opts), fields(offset, limit, found))]
pub async fn get_all_channels(
    pool: &PgPool,
    offset: i64,
    limit: i64,
    opts: &ChannelSearchOpts,
) -> Result<ChannelListWithTeamData, StoreError> {
    let rows = sqlx::query_as!(
        ChannelWithTeamDataRow,
        r#"
        SELECT
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
               ), false) AS "policy_is_active!",
               CASE WHEN $3 THEN (
                   SELECT rpc.policyid FROM retentionpolicieschannels rpc
                    WHERE rpc.channelid = c.id
               ) END AS "policyid",
               COALESCE(t.displayname, '') AS "teamdisplayname!",
               COALESCE(t.name, '') AS "teamname!",
               COALESCE(t.updateat, 0) AS "teamupdateat!"
          FROM channels c
          JOIN teams t ON t.id = c.teamid
         WHERE c.type IN ('P', 'O')
           AND ($4 OR c.deleteat = 0)
           AND ($5 = '' OR c.id NOT IN (
                   SELECT gc.channelid FROM groupchannels gc
                    WHERE gc.groupid = $5 AND gc.deleteat = 0))
           AND (NOT $6 OR c.groupconstrained = true)
           AND ($6 OR NOT $7 OR c.groupconstrained IS DISTINCT FROM true)
           AND (cardinality($8::text[]) = 0 OR c.name <> ALL($8::text[]))
           AND (NOT $9 OR NOT EXISTS (
                   SELECT 1 FROM retentionpolicieschannels rpc
                    WHERE rpc.channelid = c.id))
           AND (NOT $10 OR c.id NOT IN (
                   SELECT acp.id FROM accesscontrolpolicies acp WHERE acp.type = 'channel'))
           AND ($10 OR NOT $11 OR EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp WHERE acp.id = c.id))
         ORDER BY c.displayname, t.displayname
         LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
        opts.include_policy_id,
        opts.include_deleted,
        opts.not_associated_to_group,
        opts.group_constrained,
        opts.exclude_group_constrained,
        &opts.exclude_channel_names[..],
        opts.exclude_policy_constrained,
        opts.exclude_access_control_policy_enforced,
        opts.access_control_policy_enforced,
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to get all channels".to_owned(),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());
    let channels: Vec<_> = rows
        .into_iter()
        .map(channel_with_team_data_from_row)
        .collect::<Result<_, _>>()?;
    Ok(ChannelListWithTeamData(channels))
}

/// Port of `SqlChannelStore.GetAllChannelsCount` (channel_store.go:1363) — the same query with
/// `count(c.Id)` and **without the `Teams` join**; see [`get_all_channels`] for what that costs.
///
/// The `LEFT JOIN RetentionPoliciesChannels` Go adds when `IncludePolicyID` is set cannot change
/// a count (`ChannelId` is that table's primary key), so it has no counterpart here.
#[tracing::instrument(skip(pool, opts))]
pub async fn get_all_channels_count(
    pool: &PgPool,
    opts: &ChannelSearchOpts,
) -> Result<i64, StoreError> {
    sqlx::query_scalar!(
        r#"
        SELECT COUNT(c.id) AS "count!"
          FROM channels c
         WHERE c.type IN ('P', 'O')
           AND ($1 OR c.deleteat = 0)
           AND ($2 = '' OR c.id NOT IN (
                   SELECT gc.channelid FROM groupchannels gc
                    WHERE gc.groupid = $2 AND gc.deleteat = 0))
           AND (NOT $3 OR c.groupconstrained = true)
           AND ($3 OR NOT $4 OR c.groupconstrained IS DISTINCT FROM true)
           AND (cardinality($5::text[]) = 0 OR c.name <> ALL($5::text[]))
           AND (NOT $6 OR NOT EXISTS (
                   SELECT 1 FROM retentionpolicieschannels rpc
                    WHERE rpc.channelid = c.id))
           AND (NOT $7 OR c.id NOT IN (
                   SELECT acp.id FROM accesscontrolpolicies acp WHERE acp.type = 'channel'))
           AND ($7 OR NOT $8 OR EXISTS (
                   SELECT 1 FROM accesscontrolpolicies acp WHERE acp.id = c.id))
        "#,
        opts.include_deleted,
        opts.not_associated_to_group,
        opts.group_constrained,
        opts.exclude_group_constrained,
        &opts.exclude_channel_names[..],
        opts.exclude_policy_constrained,
        opts.exclude_access_control_policy_enforced,
        opts.access_control_policy_enforced,
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to count all channels".to_owned(),
        source,
    })
}

/// The default page size `channelSearchQuery` falls back to when `PerPage` is nil
/// (channel_store.go:3649) — **100**, not [`mm_model::channel_search::CHANNEL_SEARCH_DEFAULT_LIMIT`],
/// which is 50 and belongs to `searchGroupChannels`.
const CHANNEL_SEARCH_QUERY_DEFAULT_LIMIT: i64 = 100;

/// Port of `SqlChannelStore.SearchAllChannels` (channel_store.go:3778) and `channelSearchQuery`
/// (channel_store.go:3644) — the system console's channel search.
///
/// Returns the page and the total. **The total means two different things**: Go runs the second
/// `count(*)` query only when `IsPaginated()` — both `Page` and `PerPage` present — and
/// otherwise sets it to `len(channels)`, the size of the page it already has
/// (channel_store.go:3802). So an unpaginated search reports the number of rows returned, which
/// is a ceiling rather than a total whenever the limit bit. The handler writes the count on the
/// wire only under the paginated condition, so the difference is not observable through
/// `searchAllChannels`; it is the store's contract all the same.
///
/// # Order, and the absence of a tie-break
///
/// `ORDER BY c.DisplayName, t.DisplayName`, exactly as [`get_all_channels`], with the same
/// missing third key. See that function.
///
/// # `Deleted` beats `IncludeDeleted`
///
/// Go's chain is `if Deleted { DeleteAt <> 0 } else if !IncludeDeleted { DeleteAt = 0 }`. So
/// `deleted` alone gives the archived channels *only*, and `deleted` with `include_deleted` is
/// still archived-only — the second flag never gets a say. The predicate below keeps that order.
///
/// # `public` and `private` are three cases, not two flags
///
/// `public && !private` joins `PublicChannels`, Go's denormalised shadow of the open channels;
/// `private && !public` is `c.Type = 'P'`; **everything else**, including both set and neither
/// set, is `c.Type IN ('O','P')`. Setting both is therefore the same request as setting neither.
///
/// # `policy_id` filtering is not reached from the REST API
///
/// `channelSearchQuery`'s first branch — `PolicyID != ""`, an inner join that narrows to one
/// retention policy — has no caller in api4: `searchAllChannels` never sets the field, and
/// `ChannelSearch` has no json tag for it. Only the `exclude_policy_constrained` and
/// `include_policy_id` branches below are reachable, so only those are ported; a future data
/// retention route wanting the first must add it and a test that sends it.
#[tracing::instrument(skip(pool, opts), fields(term_len = term.len(), found, total))]
pub async fn search_all_channels(
    pool: &PgPool,
    term: &str,
    opts: &ChannelSearchOpts,
) -> Result<(ChannelListWithTeamData, i64), StoreError> {
    let sanitized = sanitize_search_term(term);
    let has_search = !sanitized.is_empty();
    let like_term = if has_search {
        wildcard_search_term(&sanitized)
    } else {
        String::new()
    };
    let fulltext_term = build_fulltext_term(term);
    let text_config = default_text_search_config(pool).await?;

    let limit = opts.per_page.unwrap_or(CHANNEL_SEARCH_QUERY_DEFAULT_LIMIT);
    let paginated = opts.page.is_some() && opts.per_page.is_some();
    let offset = match (opts.page, opts.per_page) {
        (Some(page), Some(per_page)) => page.saturating_mul(per_page),
        _ => 0,
    };
    // Go interpolates `fmt.Sprintf("%q", ...)` (channel_store.go:3768) and lets Postgres parse
    // the result as jsonb; binding the same value as a JSON string reaches the identical
    // document. For the 26-character base32 ids this field carries, Go's quoting and JSON's are
    // the same bytes anyway — they diverge only on non-ASCII, which no policy id contains.
    let parent_policy_json =
        serde_json::Value::String(opts.parent_access_control_policy_id.clone());

    let rows = sqlx::query_as!(
        ChannelWithTeamDataRow,
        r#"
        SELECT
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
               ), false) AS "policy_is_active!",
               CASE WHEN $3 THEN (
                   SELECT rpc.policyid FROM retentionpolicieschannels rpc
                    WHERE rpc.channelid = c.id
               ) END AS "policyid",
               COALESCE(t.displayname, '') AS "teamdisplayname!",
               COALESCE(t.name, '') AS "teamname!",
               COALESCE(t.updateat, 0) AS "teamupdateat!"
          FROM channels c
          JOIN teams t ON t.id = c.teamid
         WHERE (($5 AND c.deleteat <> 0) OR (NOT $5 AND ($6 OR c.deleteat = 0)))
           AND (NOT $4 OR NOT EXISTS (
                   SELECT 1 FROM retentionpolicieschannels rpc
                    WHERE rpc.channelid = c.id))
           AND (NOT $7
                OR LOWER(c.name) LIKE LOWER($8) ESCAPE '*'
                OR LOWER(c.displayname) LIKE LOWER($8) ESCAPE '*'
                OR LOWER(c.purpose) LIKE LOWER($8) ESCAPE '*'
                OR ($9 AND LOWER(c.id) LIKE LOWER($8) ESCAPE '*')
                OR to_tsvector($10::text::regconfig,
                               c.name || ' ' || c.displayname || ' ' || c.purpose)
                   @@ to_tsquery($10::text::regconfig, $11))
           AND (cardinality($12::text[]) = 0 OR c.name <> ALL($12::text[]))
           AND ($13 = '' OR c.id NOT IN (
                   SELECT gc.channelid FROM groupchannels gc
                    WHERE gc.groupid = $13 AND gc.deleteat = 0))
           AND (cardinality($14::text[]) = 0 OR c.teamid = ANY($14::text[]))
           AND (NOT $15 OR c.groupconstrained = true)
           AND ($15 OR NOT $16 OR c.groupconstrained IS DISTINCT FROM true)
           AND (CASE
                  WHEN $17 AND NOT $18
                    THEN EXISTS (SELECT 1 FROM publicchannels pc WHERE pc.id = c.id)
                  WHEN $18 AND NOT $17 THEN c.type = 'P'
                  ELSE c.type IN ('O', 'P')
                END)
           AND (NOT $19
                OR EXISTS (SELECT 1 FROM sharedchannels sc
                            WHERE sc.channelid = c.id AND sc.home = true)
                OR NOT EXISTS (SELECT 1 FROM sharedchannels sc WHERE sc.channelid = c.id))
           AND (CASE
                  WHEN $20 THEN c.id NOT IN (
                      SELECT acp.id FROM accesscontrolpolicies acp WHERE acp.type = 'channel')
                  WHEN $21 <> '' THEN c.id IN (
                      SELECT acp.id FROM accesscontrolpolicies acp
                       WHERE acp.type = 'channel' AND acp.data->'imports' @> $22::jsonb)
                  WHEN $23
                    THEN EXISTS (SELECT 1 FROM accesscontrolpolicies acp WHERE acp.id = c.id)
                  ELSE true
                END)
         ORDER BY c.displayname, t.displayname
         LIMIT $1 OFFSET $2
        "#,
        limit,
        offset,
        opts.include_policy_id,
        opts.exclude_policy_constrained,
        opts.deleted,
        opts.include_deleted,
        has_search,
        like_term,
        opts.include_search_by_id,
        text_config,
        fulltext_term,
        &opts.exclude_channel_names[..],
        opts.not_associated_to_group,
        &opts.team_ids[..],
        opts.group_constrained,
        opts.exclude_group_constrained,
        opts.public,
        opts.private,
        opts.exclude_remote,
        opts.exclude_access_control_policy_enforced,
        opts.parent_access_control_policy_id,
        &parent_policy_json,
        opts.access_control_policy_enforced,
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
        .map(channel_with_team_data_from_row)
        .collect::<Result<_, _>>()?;

    if !paginated {
        // Go's `else` branch (channel_store.go:3802): the total of an unpaginated search is the
        // **length of the page**, not zero and not a second query. Unobservable through
        // `searchAllChannels`, whose handler writes the count only when the same condition holds
        // — but this is the store's contract, and a future caller reading it would be told 0
        // where Go says 4.
        let total = i64::try_from(channels.len()).unwrap_or(i64::MAX);
        tracing::Span::current().record("total", total);
        return Ok((ChannelListWithTeamData(channels), total));
    }

    let total = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!"
          FROM channels c
          JOIN teams t ON t.id = c.teamid
         WHERE (($1 AND c.deleteat <> 0) OR (NOT $1 AND ($2 OR c.deleteat = 0)))
           AND (NOT $3 OR NOT EXISTS (
                   SELECT 1 FROM retentionpolicieschannels rpc
                    WHERE rpc.channelid = c.id))
           AND (NOT $4
                OR LOWER(c.name) LIKE LOWER($5) ESCAPE '*'
                OR LOWER(c.displayname) LIKE LOWER($5) ESCAPE '*'
                OR LOWER(c.purpose) LIKE LOWER($5) ESCAPE '*'
                OR ($6 AND LOWER(c.id) LIKE LOWER($5) ESCAPE '*')
                OR to_tsvector($7::text::regconfig,
                               c.name || ' ' || c.displayname || ' ' || c.purpose)
                   @@ to_tsquery($7::text::regconfig, $8))
           AND (cardinality($9::text[]) = 0 OR c.name <> ALL($9::text[]))
           AND ($10 = '' OR c.id NOT IN (
                   SELECT gc.channelid FROM groupchannels gc
                    WHERE gc.groupid = $10 AND gc.deleteat = 0))
           AND (cardinality($11::text[]) = 0 OR c.teamid = ANY($11::text[]))
           AND (NOT $12 OR c.groupconstrained = true)
           AND ($12 OR NOT $13 OR c.groupconstrained IS DISTINCT FROM true)
           AND (CASE
                  WHEN $14 AND NOT $15
                    THEN EXISTS (SELECT 1 FROM publicchannels pc WHERE pc.id = c.id)
                  WHEN $15 AND NOT $14 THEN c.type = 'P'
                  ELSE c.type IN ('O', 'P')
                END)
           AND (NOT $16
                OR EXISTS (SELECT 1 FROM sharedchannels sc
                            WHERE sc.channelid = c.id AND sc.home = true)
                OR NOT EXISTS (SELECT 1 FROM sharedchannels sc WHERE sc.channelid = c.id))
           AND (CASE
                  WHEN $17 THEN c.id NOT IN (
                      SELECT acp.id FROM accesscontrolpolicies acp WHERE acp.type = 'channel')
                  WHEN $18 <> '' THEN c.id IN (
                      SELECT acp.id FROM accesscontrolpolicies acp
                       WHERE acp.type = 'channel' AND acp.data->'imports' @> $19::jsonb)
                  WHEN $20
                    THEN EXISTS (SELECT 1 FROM accesscontrolpolicies acp WHERE acp.id = c.id)
                  ELSE true
                END)
        "#,
        opts.deleted,
        opts.include_deleted,
        opts.exclude_policy_constrained,
        has_search,
        like_term,
        opts.include_search_by_id,
        text_config,
        fulltext_term,
        &opts.exclude_channel_names[..],
        opts.not_associated_to_group,
        &opts.team_ids[..],
        opts.group_constrained,
        opts.exclude_group_constrained,
        opts.public,
        opts.private,
        opts.exclude_remote,
        opts.exclude_access_control_policy_enforced,
        opts.parent_access_control_policy_id,
        &parent_policy_json,
        opts.access_control_policy_enforced,
    )
    .fetch_one(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: "failed to find Channels".to_owned(),
        source,
    })?;

    tracing::Span::current().record("total", total);
    Ok((ChannelListWithTeamData(channels), total))
}

/// Port of `SqlChannelStore.SearchGroupChannels` and `searchGroupChannelsQuery`
/// (channel_store.go:3977, :3943) — the group-message channels the caller is in whose **member
/// usernames** match every word of the term.
///
/// # The term matches a joined roster, not a channel name
///
/// A group message has no meaningful `DisplayName`, so the predicate is built over
/// `ARRAY_TO_STRING(ARRAY_AGG(u.Username), ', ')` — the aggregated usernames of *all* the
/// channel's members, the caller's included — and every whitespace-separated word of the term
/// must be a substring of that one string. So "alice bob" finds the conversation containing both,
/// in either order, and "ali ob" finds it too.
///
/// The term is lower-cased **before** it is split, and the LIKE has no `lower()` on either side:
/// Mattermost usernames are already lower-case, so the comparison works, but a term is only
/// matched case-insensitively because it was folded here.
///
/// # A whitespace-only term returns every group message, up to 50
///
/// `strings.Fields` of `"   "` is empty, so the `HAVING` is squirrel's empty `And{}`, which
/// renders as **`(1=1)`** (squirrel expr.go:14) rather than disappearing. The result is an
/// unfiltered list of the caller's group messages. The empty term never gets this far —
/// `App.SearchGroupChannels` (app/channel.go:3533) short-circuits `""` to an empty list before
/// the store is called — but a single space does. Both are pinned by tests.
///
/// # Escaping is `\`, not `*`
///
/// `sanitizeSearchTerm(term, "\\")` and a LIKE with **no `ESCAPE` clause**, so Postgres' default
/// backslash applies. Every other channel search in this file escapes with `*` and says so in the
/// clause; copying that here would leave `%` and `_` in a username search unescaped.
///
/// # The outer query has no `ORDER BY`
///
/// `SELECT … FROM Channels WHERE Id IN (…)` and nothing more, so the order is the plan's. The
/// inner `LIMIT 50` is applied to the id subquery, whose own order is equally unspecified —
/// which means *which* 50 is unspecified once a caller has more than fifty group messages.
/// Reproduced as written; a test that depended on the order would be testing the planner.
#[tracing::instrument(skip(pool), fields(user_id = %user_id, terms, found))]
pub async fn search_group_channels(
    pool: &PgPool,
    user_id: &str,
    term: &str,
) -> Result<ChannelList, StoreError> {
    let terms: Vec<String> = mm_model::utils::go_to_lower(term)
        .split_whitespace()
        .map(|word| format!("%{}%", sanitize_search_term_with_backslash(word)))
        .collect();
    tracing::Span::current().record("terms", terms.len());

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
         WHERE ch.id IN (
                   SELECT cc.id
                     FROM (SELECT c.id
                             FROM channels c
                             JOIN channelmembers cm ON c.id = cm.channelid
                             JOIN users u ON u.id = cm.userid
                            WHERE c.type = 'G' AND u.id = $1
                            GROUP BY c.id) cc
                     JOIN channelmembers cm ON cc.id = cm.channelid
                     JOIN users u ON u.id = cm.userid
                    GROUP BY cc.id
                   HAVING ARRAY_TO_STRING(ARRAY_AGG(u.username), ', ') ~~ ALL($2::text[])
                    LIMIT 50)
        "#,
        user_id,
        &terms[..],
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find Channels with userId={user_id}"),
        source,
    })?;

    tracing::Span::current().record("found", rows.len());
    let channels: Vec<_> = rows
        .into_iter()
        .map(channel_from_row)
        .collect::<Result<_, _>>()?;
    Ok(ChannelList(channels))
}

/// [`sanitize_search_term`] with Go's **other** escape character.
///
/// `sanitizeSearchTerm(term, "\\")` (sqlstore/utils.go:62) — the same two steps, removing every
/// backslash first and then escaping `%` and `_` with one. Only `searchGroupChannelsQuery` uses
/// it; everything else in this file escapes with `*` and spells `ESCAPE '*'` in the clause.
fn sanitize_search_term_with_backslash(term: &str) -> String {
    let mut out = term.replace('\\', "");
    for c in ['%', '_'] {
        out = out.replace(c, &format!("\\{c}"));
    }
    out
}

#[cfg(test)]
mod search_term_tests {
    use super::*;

    /// `sanitizeSearchTerm(term, "\\")` — the group-channel search's escape character, which is
    /// not the `*` every other search in this file uses.
    ///
    /// The order is Go's and it is the whole point: every backslash is **removed first**, and
    /// only then are `%` and `_` escaped with one. A term of a single backslash therefore
    /// sanitises to the empty string rather than to an escaped backslash.
    #[test]
    fn the_backslash_variant_strips_before_it_escapes() {
        assert_eq!(sanitize_search_term_with_backslash("alice"), "alice");
        assert_eq!(sanitize_search_term_with_backslash(""), "");
        // Stripped, not escaped.
        assert_eq!(sanitize_search_term_with_backslash("\\"), "");
        assert_eq!(sanitize_search_term_with_backslash("a\\b"), "ab");
        // The two LIKE metacharacters, escaped with the backslash Postgres defaults to.
        assert_eq!(sanitize_search_term_with_backslash("50%"), "50\\%");
        assert_eq!(sanitize_search_term_with_backslash("a_b"), "a\\_b");
        assert_eq!(sanitize_search_term_with_backslash("%_%"), "\\%\\_\\%");
        // A backslash the caller supplied cannot smuggle an escape through: it goes first, so
        // `\\%` is `%` escaped by *this* function rather than a literal percent sign.
        assert_eq!(sanitize_search_term_with_backslash("\\%"), "\\%");
        // `*` is not special here, unlike in its sibling.
        assert_eq!(sanitize_search_term_with_backslash("*"), "*");
    }

    /// Its sibling, for contrast — same two steps, different character, and `*` is what vanishes.
    #[test]
    fn the_star_variant_is_the_same_shape_with_a_different_character() {
        assert_eq!(sanitize_search_term("*"), "");
        assert_eq!(sanitize_search_term("50%"), "50*%");
        assert_eq!(sanitize_search_term("\\"), "\\");
    }
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
