//! The plugin API's channel, member, sidebar, post-list, reaction and emoji methods
//! (app/plugin_api.go), each a thin wrapper over the app function a REST route already uses —
//! docs/PLUGIN_PLAN.md, Phase 6.
//!
//! The trait methods in `crate::plugin_api` delegate here one line each; what each answers, and
//! why, is on the function below.
//!
//! # A plugin reaches inputs REST never sends
//!
//! REST clamps a page size to 200 and floors a page at 0 before any of these app functions
//! runs; the plugin API passes both through. So the store refusals Go makes of such values are
//! answered by the app functions now: `GetPostsForChannel` with more than 1000 per page is
//! `app.post.get_posts.app_error` at 400, and `GetPostsBefore`/`GetPostsAfter` with a negative
//! page or size is `app.post.get_posts_around.get.app_error` at 400. Every `page * perPage`
//! wraps as Go's `int` does.
//!
//! # Go bugs kept
//!
//! `GetChannelStats` answers `GuestCount` from a **second** `GetChannelMemberCount`, so it is the
//! member count, guests or not; and `GetChannelMembersForUser` ignores its team id ("never used
//! in the SQL query").
//!
//! # What is not implemented, per call
//!
//! Decided before anything is written: `SearchPostsInTeam` with a term other than `*` (the
//! store's search without a user is not ported, [D-1050]); a leave, removal or
//! deletion the REST route would forward (a guest, a group-constrained or shared channel); a
//! search whose `in:` names a DM this server cannot create; and `GetEmojiImage` from a file
//! backend this server does not read.

use std::collections::HashMap;

use mm_model::channel::{CHANNEL_TYPE_SPACE, ChannelSearchOpts};
use mm_model::emoji::{EMOJI_SORT_BY_NAME, Emoji};
use mm_model::post_search_results::PostSearchResults;
use mm_model::sidebar_category::{
    OrderedSidebarCategories, SidebarCategory, SidebarCategoryWithChannels,
};
use mm_model::utils::{AppError, StringMap};
use mm_plugin::rpc::NotImplemented;
use mm_plugin::wire::model as wire;
use mm_plugin::wire::plugin as api;
use mm_store::post_store::{GetPostsAroundOptions, GetPostsOptions};

use super::AppPluginApi;
use crate::channel_member::MemberWrite;
use crate::plugin_api_wire::post_list_for_plugin;
use crate::plugin_hooks::{
    HookContext, channel_from_wire, channel_member_to_wire, channel_to_wire, reaction_to_wire,
};
use crate::post::PrepareError;
use crate::post_search::PostSearchError;

/// `SearchPostsInTeamForUser`'s `PerPage` when the plugin sends none. The database search reads
/// no page size, so it changes nothing here; kept so the defaults read as Go's.
pub const SEARCH_DEFAULT_PER_PAGE: i64 = 100;

/// `PluginAPI.rejectSpaceChannel`'s refusal (app/plugin_api.go:760).
fn space_notify_props_error() -> Box<AppError> {
    AppError::boxed(
        "PluginAPI.rejectSpaceChannel",
        "plugin_api.channel.space_notify_props.app_error",
        None,
        "",
        400,
    )
}

/// A gob `map[string]string` as the model's map.
fn string_map(map: &HashMap<String, String>) -> StringMap {
    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// `PluginAPI.GetChannelStats`' answer: `GuestCount` is the member count again — Go calls
/// `GetChannelMemberCount` twice — and the pinned-post and file counts stay zero.
pub fn channel_stats(channel_id: &str, member_count: i64, second_count: i64) -> wire::ChannelStats {
    wire::ChannelStats {
        channel_id: channel_id.to_owned(),
        member_count,
        guest_count: second_count,
        pinned_post_count: 0,
        files_count: 0,
    }
}

/// `SearchPostsInTeamForUser`'s unpacking of `model.SearchParameter`: each nil pointer is its
/// default — no terms, offset 0, an AND search, page 0, 100 per page, archived channels left out.
#[derive(Debug, PartialEq, Eq)]
pub struct SearchForUser {
    pub terms: String,
    pub time_zone_offset: i64,
    pub is_or_search: bool,
    pub page: i64,
    pub per_page: i64,
    pub include_deleted_channels: bool,
}

pub fn search_for_user(parameter: &wire::SearchParameter) -> SearchForUser {
    SearchForUser {
        terms: parameter.terms.clone().unwrap_or_default(),
        time_zone_offset: parameter.time_zone_offset.unwrap_or(0),
        is_or_search: parameter.is_or_search.unwrap_or(false),
        page: parameter.page.unwrap_or(0),
        per_page: parameter.per_page.unwrap_or(SEARCH_DEFAULT_PER_PAGE),
        include_deleted_channels: parameter.include_deleted_channels.unwrap_or(false),
    }
}

/// How `App.SearchPostsInTeam` (app/post.go:2242) answers a plugin's search here.
#[derive(Debug, PartialEq, Eq)]
pub enum InTeamSearch {
    /// `EnablePostSearch` off: Go's 501, before anything else.
    Disabled,
    /// Every element's terms are `*`, which `searchPostsInTeam` skips: no store call, an empty
    /// list.
    Empty,
    /// A real term reaches `Post().Search` with `SearchWithoutUserId`, which is not ported.
    Unported,
}

pub fn in_team_search(enabled: bool, params: &[wire::SearchParams]) -> InTeamSearch {
    if !enabled {
        InTeamSearch::Disabled
    } else if params.iter().all(|p| p.terms == "*") {
        InTeamSearch::Empty
    } else {
        InTeamSearch::Unported
    }
}

/// An emoji as gob sends it (`model.Emoji`, emoji.go:28).
pub fn emoji_to_wire(emoji: &Emoji) -> wire::Emoji {
    wire::Emoji {
        id: emoji.id.clone(),
        create_at: emoji.create_at,
        update_at: emoji.update_at,
        delete_at: emoji.delete_at,
        creator_id: emoji.creator_id.clone(),
        name: emoji.name.clone(),
    }
}

/// A category as gob sends it. An empty channel list and a nil one are one value on the wire.
pub fn sidebar_category_to_wire(
    category: &SidebarCategoryWithChannels,
) -> wire::SidebarCategoryWithChannels {
    let c = &category.category;
    wire::SidebarCategoryWithChannels {
        sidebar_category: wire::SidebarCategory {
            id: c.id.clone(),
            user_id: c.user_id.clone(),
            team_id: c.team_id.clone(),
            sort_order: c.sort_order,
            sorting: c.sorting.clone(),
            r#type: c.category_type.clone(),
            display_name: c.display_name.clone(),
            muted: c.muted,
            collapsed: c.collapsed,
        },
        channels: category.channel_ids.clone().unwrap_or_default(),
    }
}

/// A category as the plugin sent it: gob delivers an empty list as Go's nil, so it is `None`.
pub fn sidebar_category_from_wire(
    wire: &wire::SidebarCategoryWithChannels,
) -> SidebarCategoryWithChannels {
    let c = &wire.sidebar_category;
    SidebarCategoryWithChannels {
        category: SidebarCategory {
            id: c.id.clone(),
            user_id: c.user_id.clone(),
            team_id: c.team_id.clone(),
            sort_order: c.sort_order,
            sorting: c.sorting.clone(),
            category_type: c.r#type.clone(),
            display_name: c.display_name.clone(),
            muted: c.muted,
            collapsed: c.collapsed,
        },
        channel_ids: (!wire.channels.is_empty()).then(|| wire.channels.clone()),
    }
}

fn ordered_sidebar_categories_to_wire(
    ordered: &OrderedSidebarCategories,
) -> wire::OrderedSidebarCategories {
    wire::OrderedSidebarCategories {
        categories: ordered
            .categories
            .iter()
            .flatten()
            .map(sidebar_category_to_wire)
            .collect(),
        order: ordered.order.clone().unwrap_or_default(),
    }
}

/// Search results as gob sends them, `ForPlugin`: the embedded list's posts are.
fn search_results_to_wire(results: &PostSearchResults) -> wire::PostSearchResults {
    wire::PostSearchResults {
        post_list: results
            .post_list
            .as_ref()
            .map(|list| Box::new(post_list_for_plugin(list))),
        matches: results
            .matches
            .iter()
            .flatten()
            // A nil term list crosses gob as an empty one.
            .map(|(id, terms)| (id.clone(), terms.clone().unwrap_or_default()))
            .collect(),
    }
}

impl AppPluginApi {
    /// Port of `PluginAPI.rejectSpaceChannel` (app/plugin_api.go:760): a space backing channel
    /// is refused; a lookup that fails other than as a 404 fails closed with its own error.
    async fn reject_space_channel(&self, channel_id: &str) -> Result<(), Box<AppError>> {
        match self
            .app
            .get_channel_of_type(channel_id, CHANNEL_TYPE_SPACE)
            .await
        {
            Ok(_) => Err(space_notify_props_error()),
            Err(err) if err.status_code != 404 => Err(err),
            Err(_) => Ok(()),
        }
    }

    /// A post list's two returns, `ForPlugin`.
    fn reply_post_list(
        &self,
        result: Result<mm_model::post_list::PostList, Box<AppError>>,
    ) -> (Option<Box<wire::PostList>>, Option<Box<wire::AppError>>) {
        self.reply(result, |list| post_list_for_plugin(&list))
    }

    // -- channels -------------------------------------------------------------------------------

    /// Port of `PluginAPI.GetChannelsForTeamForUser` (app/plugin_api.go:567): the user's
    /// channels on the team **and** their DMs and group messages, archived ones when asked
    /// (with no `LastDeleteAt` floor). A team with nothing is `GetChannels`' 404.
    pub(super) async fn channels_get_channels_for_team_for_user(
        &self,
        args: api::Z_GetChannelsForTeamForUserArgs,
    ) -> Result<api::Z_GetChannelsForTeamForUserReturns, NotImplemented> {
        let options = ChannelSearchOpts {
            include_deleted: args.c,
            last_delete_at: 0,
            ..ChannelSearchOpts::default()
        };
        let result = self
            .app
            .get_channels_for_team_for_user(&args.a, &args.b, &options)
            .await
            .map(|list| list.0);
        let (a, b) = self.reply_list(result, channel_to_wire);
        Ok(api::Z_GetChannelsForTeamForUserReturns { a, b })
    }

    /// Port of `PluginAPI.GetPublicChannelsForTeam` (app/plugin_api.go:495): the offset is
    /// `page * perPage`, wrapping.
    pub(super) async fn channels_get_public_channels_for_team(
        &self,
        args: api::Z_GetPublicChannelsForTeamArgs,
    ) -> Result<api::Z_GetPublicChannelsForTeamReturns, NotImplemented> {
        let result = self
            .app
            .get_public_channels_for_team(&args.a, args.b.wrapping_mul(args.c), args.c)
            .await
            .map(|list| list.0);
        let (a, b) = self.reply_list(result, channel_to_wire);
        Ok(api::Z_GetPublicChannelsForTeamReturns { a, b })
    }

    /// Port of `PluginAPI.SearchChannels` (app/plugin_api.go:610): public channels, archived
    /// ones included, the term trimmed.
    pub(super) async fn channels_search_channels(
        &self,
        args: api::Z_SearchChannelsArgs,
    ) -> Result<api::Z_SearchChannelsReturns, NotImplemented> {
        let result = self
            .app
            .search_channels(&args.a, &args.b)
            .await
            .map(|list| list.0);
        let (a, b) = self.reply_list(result, channel_to_wire);
        Ok(api::Z_SearchChannelsReturns { a, b })
    }

    /// Port of `PluginAPI.UpdateChannel` (app/plugin_api.go:598): `App.UpdateChannel` with the
    /// plugin's channel taken whole — no patch, no system posts — answered as stored. A nil
    /// channel (Go dereferences it and panics) is the zero channel, which is no channel.
    pub(super) async fn channels_update_channel(
        &self,
        args: api::Z_UpdateChannelArgs,
    ) -> Result<api::Z_UpdateChannelReturns, NotImplemented> {
        let mut channel = args.a.as_deref().map(channel_from_wire).unwrap_or_default();
        let result = self
            .app
            .update_channel(&HookContext::default(), &mut channel)
            .await
            .map(|()| channel);
        let (a, b) = self.reply(result, |c| channel_to_wire(&c));
        Ok(api::Z_UpdateChannelReturns { a, b })
    }

    /// Port of `PluginAPI.DeleteChannel` (app/plugin_api.go:478): the channel resolved as
    /// `resolveChannel` does, then archived by **nobody** — so no "archived the channel" post.
    pub(super) async fn channels_delete_channel(
        &self,
        args: api::Z_DeleteChannelArgs,
    ) -> Result<api::Z_DeleteChannelReturns, NotImplemented> {
        let result = match self.resolve_channel(&args.a).await {
            Ok(channel) => self
                .app
                .delete_channel(&HookContext::default(), &channel, "")
                .await
                .map(|_| ()),
            Err(err) => Err(err),
        };
        Ok(api::Z_DeleteChannelReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.GetChannelStats` (app/plugin_api.go:578); see [`channel_stats`]. A
    /// channel that is not there counts zero rather than failing.
    pub(super) async fn channels_get_channel_stats(
        &self,
        args: api::Z_GetChannelStatsArgs,
    ) -> Result<api::Z_GetChannelStatsReturns, NotImplemented> {
        let counts = match self.app.get_channel_member_count(&args.a).await {
            Ok(members) => match self.app.get_channel_member_count(&args.a).await {
                Ok(again) => Ok((members, again)),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        };
        let (a, b) = self.reply(counts, |(members, again)| {
            channel_stats(&args.a, members, again)
        });
        Ok(api::Z_GetChannelStatsReturns { a, b })
    }

    /// Port of `PluginAPI.GetChannelMembers` (app/plugin_api.go:721): `GetChannelMembersPage`,
    /// in no promised order.
    pub(super) async fn channels_get_channel_members(
        &self,
        args: api::Z_GetChannelMembersArgs,
    ) -> Result<api::Z_GetChannelMembersReturns, NotImplemented> {
        let result = self
            .app
            .get_channel_members_page(&args.a, args.b, args.c)
            .await;
        let (a, b) = self.reply_list(result, channel_member_to_wire);
        Ok(api::Z_GetChannelMembersReturns { a, b })
    }

    /// Port of `PluginAPI.GetChannelMembersByIds` (app/plugin_api.go:725).
    pub(super) async fn channels_get_channel_members_by_ids(
        &self,
        args: api::Z_GetChannelMembersByIdsArgs,
    ) -> Result<api::Z_GetChannelMembersByIdsReturns, NotImplemented> {
        let result = self.app.get_channel_members_by_ids(&args.a, &args.b).await;
        let (a, b) = self.reply_list(result, channel_member_to_wire);
        Ok(api::Z_GetChannelMembersByIdsReturns { a, b })
    }

    /// Port of `PluginAPI.GetChannelMembersForUser` (app/plugin_api.go:729): the team id is
    /// **ignored**, so every membership of the user on every team, in channel-id order.
    pub(super) async fn channels_get_channel_members_for_user(
        &self,
        args: api::Z_GetChannelMembersForUserArgs,
    ) -> Result<api::Z_GetChannelMembersForUserReturns, NotImplemented> {
        let result = self
            .app
            .get_channel_members_for_user_with_pagination(&args.b, args.c, args.d)
            .await;
        let (a, b) = self.reply_list(result, channel_member_to_wire);
        Ok(api::Z_GetChannelMembersForUserReturns { a, b })
    }

    /// Port of `PluginAPI.UpdateChannelMemberRoles` (app/plugin_api.go:735).
    pub(super) async fn channels_update_channel_member_roles(
        &self,
        args: api::Z_UpdateChannelMemberRolesArgs,
    ) -> Result<api::Z_UpdateChannelMemberRolesReturns, NotImplemented> {
        let result = self
            .app
            .update_channel_member_roles(&args.a, &args.b, &args.c)
            .await;
        let (a, b) = self.reply(result, |m| channel_member_to_wire(&m));
        Ok(api::Z_UpdateChannelMemberRolesReturns { a, b })
    }

    /// Port of `PluginAPI.UpdateChannelMemberNotifications` (app/plugin_api.go:739): a space
    /// channel is refused first; then the ten known keys are merged, unvalidated, as REST does.
    pub(super) async fn channels_update_channel_member_notifications(
        &self,
        args: api::Z_UpdateChannelMemberNotificationsArgs,
    ) -> Result<api::Z_UpdateChannelMemberNotificationsReturns, NotImplemented> {
        let result = match self.reject_space_channel(&args.a).await {
            Ok(()) => {
                self.app
                    .update_channel_member_notify_props(&string_map(&args.c), &args.a, &args.b)
                    .await
            }
            Err(err) => Err(err),
        };
        let (a, b) = self.reply(result, |m| channel_member_to_wire(&m));
        Ok(api::Z_UpdateChannelMemberNotificationsReturns { a, b })
    }

    /// Port of `PluginAPI.PatchChannelMembersNotifications` (app/plugin_api.go:746): every
    /// member's channel is checked for a space first, then
    /// [`crate::App::patch_channel_members_notify_props`]. Only the error crosses.
    pub(super) async fn channels_patch_channel_members_notifications(
        &self,
        args: api::Z_PatchChannelMembersNotificationsArgs,
    ) -> Result<api::Z_PatchChannelMembersNotificationsReturns, NotImplemented> {
        for member in &args.a {
            if let Err(err) = self.reject_space_channel(&member.channel_id).await {
                return Ok(api::Z_PatchChannelMembersNotificationsReturns { a: self.wire(err) });
            }
        }
        let members: Vec<(String, String)> = args
            .a
            .iter()
            .map(|m| (m.channel_id.clone(), m.user_id.clone()))
            .collect();
        let result = self
            .app
            .patch_channel_members_notify_props(&members, &string_map(&args.b))
            .await;
        Ok(api::Z_PatchChannelMembersNotificationsReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.DeleteChannelMember` (app/plugin_api.go:771): the user **leaves** —
    /// `LeaveChannel`, with a leave post — unless the channel is a space, which is
    /// `RemoveUserFromChannel` with no post. A shape the REST route forwards is not implemented.
    pub(super) async fn channels_delete_channel_member(
        &self,
        args: api::Z_DeleteChannelMemberArgs,
    ) -> Result<api::Z_DeleteChannelMemberReturns, NotImplemented> {
        let ctx = HookContext::default();
        let result = match self.resolve_channel(&args.a).await {
            Ok(channel) if channel.is_space() => {
                self.app
                    .remove_user_from_channel(&args.b, &args.b, &channel, &ctx)
                    .await
            }
            Ok(_) => self.app.leave_channel(&ctx, &args.a, &args.b).await,
            Err(err) => Err(err),
        };
        let result = match result {
            Ok(MemberWrite::Done(())) => Ok(()),
            Ok(MemberWrite::Forward(why)) => {
                return Err(self.not_implemented("DeleteChannelMember", why));
            }
            Err(err) => Err(err),
        };
        Ok(api::Z_DeleteChannelMemberReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    // -- sidebar categories ---------------------------------------------------------------------

    /// Port of `PluginAPI.GetChannelSidebarCategories` (app/plugin_api.go:622): a read that
    /// **creates** the three default categories when the user has none on the team.
    pub(super) async fn channels_get_channel_sidebar_categories(
        &self,
        args: api::Z_GetChannelSidebarCategoriesArgs,
    ) -> Result<api::Z_GetChannelSidebarCategoriesReturns, NotImplemented> {
        let result = self
            .app
            .get_sidebar_categories_for_team_for_user(&args.a, &args.b)
            .await;
        let (a, b) = self.reply(result, |c| ordered_sidebar_categories_to_wire(&c));
        Ok(api::Z_GetChannelSidebarCategoriesReturns { a, b })
    }

    /// Port of `PluginAPI.CreateChannelSidebarCategory` (app/plugin_api.go:618). A nil category
    /// (Go dereferences it) is the zero one.
    pub(super) async fn channels_create_channel_sidebar_category(
        &self,
        args: api::Z_CreateChannelSidebarCategoryArgs,
    ) -> Result<api::Z_CreateChannelSidebarCategoryReturns, NotImplemented> {
        let category = args
            .c
            .as_deref()
            .map(sidebar_category_from_wire)
            .unwrap_or_default();
        let result = self
            .app
            .create_sidebar_category(&args.a, &args.b, &category)
            .await;
        let (a, b) = self.reply(result, |c| sidebar_category_to_wire(&c));
        Ok(api::Z_CreateChannelSidebarCategoryReturns { a, b })
    }

    /// Port of `PluginAPI.UpdateChannelSidebarCategories` (app/plugin_api.go:626). A change to a
    /// category's `muted` mutes or unmutes its channels' memberships, as on REST.
    pub(super) async fn channels_update_channel_sidebar_categories(
        &self,
        args: api::Z_UpdateChannelSidebarCategoriesArgs,
    ) -> Result<api::Z_UpdateChannelSidebarCategoriesReturns, NotImplemented> {
        let categories: Vec<SidebarCategoryWithChannels> =
            args.c.iter().map(sidebar_category_from_wire).collect();
        let result = self
            .app
            .update_sidebar_categories(&args.a, &args.b, &categories)
            .await;
        let (a, b) = self.reply_list(result, sidebar_category_to_wire);
        Ok(api::Z_UpdateChannelSidebarCategoriesReturns { a, b })
    }

    // -- posts ----------------------------------------------------------------------------------

    /// Port of `PluginAPI.GetPostsForChannel` (app/plugin_api.go:988): `GetPostsPage` for no
    /// user, not collapsed, deleted posts left out, `ForPlugin`; more than 1000 per page is 400.
    pub(super) async fn channels_get_posts_for_channel(
        &self,
        args: api::Z_GetPostsForChannelArgs,
    ) -> Result<api::Z_GetPostsForChannelReturns, NotImplemented> {
        let options = GetPostsOptions {
            channel_id: &args.a,
            user_id: "",
            page: args.b,
            per_page: args.c,
            skip_fetch_threads: false,
            collapsed_threads: false,
            include_deleted: false,
        };
        let result = self
            .app
            .get_posts_page(&HookContext::default(), options)
            .await;
        let (a, b) = self.reply_post_list(result);
        Ok(api::Z_GetPostsForChannelReturns { a, b })
    }

    /// Port of `PluginAPI.GetPostsSince` (app/plugin_api.go:964): every post whose `UpdateAt` is
    /// after the time, up to 1000, with their threads, `ForPlugin`.
    pub(super) async fn channels_get_posts_since(
        &self,
        args: api::Z_GetPostsSinceArgs,
    ) -> Result<api::Z_GetPostsSinceReturns, NotImplemented> {
        let result = self
            .app
            .get_posts_since(&HookContext::default(), &args.a, args.b, "", false, false)
            .await;
        let (a, b) = self.reply_post_list(result);
        Ok(api::Z_GetPostsSinceReturns { a, b })
    }

    /// `GetPostsBeforePost`/`GetPostsAfterPost` as the plugin API calls them.
    async fn posts_around(
        &self,
        channel_id: &str,
        post_id: &str,
        page: i64,
        per_page: i64,
        before: bool,
    ) -> (Option<Box<wire::PostList>>, Option<Box<wire::AppError>>) {
        let options = GetPostsAroundOptions {
            channel_id,
            post_id,
            user_id: "",
            page,
            per_page,
            skip_fetch_threads: false,
            collapsed_threads: false,
        };
        let result = self
            .app
            .get_posts_around_post(&HookContext::default(), options, before)
            .await;
        self.reply_post_list(result)
    }

    /// Port of `PluginAPI.GetPostsAfter` (app/plugin_api.go:972); a negative page or size is
    /// 400.
    pub(super) async fn channels_get_posts_after(
        &self,
        args: api::Z_GetPostsAfterArgs,
    ) -> Result<api::Z_GetPostsAfterReturns, NotImplemented> {
        let (a, b) = self
            .posts_around(&args.a, &args.b, args.c, args.d, false)
            .await;
        Ok(api::Z_GetPostsAfterReturns { a, b })
    }

    /// Port of `PluginAPI.GetPostsBefore` (app/plugin_api.go:980); a negative page or size is
    /// 400.
    pub(super) async fn channels_get_posts_before(
        &self,
        args: api::Z_GetPostsBeforeArgs,
    ) -> Result<api::Z_GetPostsBeforeReturns, NotImplemented> {
        let (a, b) = self
            .posts_around(&args.a, &args.b, args.c, args.d, true)
            .await;
        Ok(api::Z_GetPostsBeforeReturns { a, b })
    }

    /// Port of `PluginAPI.SearchPostsInTeam` (app/plugin_api.go:639); see [`in_team_search`].
    pub(super) async fn channels_search_posts_in_team(
        &self,
        args: api::Z_SearchPostsInTeamArgs,
    ) -> Result<api::Z_SearchPostsInTeamReturns, NotImplemented> {
        match in_team_search(self.app.config().enable_post_search, &args.b) {
            InTeamSearch::Disabled => Ok(api::Z_SearchPostsInTeamReturns {
                a: Vec::new(),
                b: self.wire(AppError::boxed(
                    "SearchPostsInTeam",
                    "store.sql_post.search.disabled",
                    None,
                    format!("teamId={}", args.a),
                    501,
                )),
            }),
            InTeamSearch::Empty => Ok(api::Z_SearchPostsInTeamReturns::default()),
            InTeamSearch::Unported => Err(self.not_implemented(
                "SearchPostsInTeam",
                "Post().Search with SearchWithoutUserId is not ported (D-1050)",
            )),
        }
    }

    /// Port of `PluginAPI.SearchPostsInTeamForUser` (app/plugin_api.go:647): `SearchPostsForUser`
    /// with [`search_for_user`]'s defaults, the team and user swapped into Go's order, the
    /// results `ForPlugin`.
    pub(super) async fn channels_search_posts_in_team_for_user(
        &self,
        args: api::Z_SearchPostsInTeamForUserArgs,
    ) -> Result<api::Z_SearchPostsInTeamForUserReturns, NotImplemented> {
        let search = search_for_user(&args.c);
        let result = self
            .app
            .search_posts_for_user(
                &HookContext::default(),
                None,
                &search.terms,
                &args.b,
                &args.a,
                search.is_or_search,
                search.include_deleted_channels,
                search.time_zone_offset,
                search.page,
            )
            .await;
        let answer = match result {
            Ok((results, _)) => api::Z_SearchPostsInTeamForUserReturns {
                a: Some(Box::new(search_results_to_wire(&results))),
                b: None,
            },
            Err(PostSearchError::App(err)) => api::Z_SearchPostsInTeamForUserReturns {
                a: None,
                b: self.wire(err),
            },
            Err(PostSearchError::Unreproducible(why)) => {
                return Err(self.not_implemented("SearchPostsInTeamForUser", why));
            }
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.GetReactions` (app/plugin_api.go:925): in the order they were made.
    pub(super) async fn channels_get_reactions(
        &self,
        args: api::Z_GetReactionsArgs,
    ) -> Result<api::Z_GetReactionsReturns, NotImplemented> {
        let result = self.app.get_reactions_for_post(&args.a).await;
        let (a, b) = self.reply_list(result, reaction_to_wire);
        Ok(api::Z_GetReactionsReturns { a, b })
    }

    // -- emoji ----------------------------------------------------------------------------------

    /// Port of `PluginAPI.GetEmoji` (app/plugin_api.go:1042): the app layer's two **403**s
    /// (custom emoji off, no file driver) are the ones a plugin sees; REST shadows the first
    /// with its own 501.
    pub(super) async fn channels_get_emoji(
        &self,
        args: api::Z_GetEmojiArgs,
    ) -> Result<api::Z_GetEmojiReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_emoji(&args.a).await, |e| emoji_to_wire(&e));
        Ok(api::Z_GetEmojiReturns { a, b })
    }

    /// Port of `PluginAPI.GetEmojiByName` (app/plugin_api.go:1038), with `GetEmoji`'s gates.
    pub(super) async fn channels_get_emoji_by_name(
        &self,
        args: api::Z_GetEmojiByNameArgs,
    ) -> Result<api::Z_GetEmojiByNameReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_emoji_by_name(&args.a).await, |e| {
            emoji_to_wire(&e)
        });
        Ok(api::Z_GetEmojiByNameReturns { a, b })
    }

    /// Port of `PluginAPI.GetEmojiList` (app/plugin_api.go:1034): no gate at all; `name` sorts
    /// by name and any other sort is none (no `ORDER BY`).
    pub(super) async fn channels_get_emoji_list(
        &self,
        args: api::Z_GetEmojiListArgs,
    ) -> Result<api::Z_GetEmojiListReturns, NotImplemented> {
        let result = self
            .app
            .get_emoji_list(args.b, args.c, args.a == EMOJI_SORT_BY_NAME)
            .await;
        let (a, b) = self.reply_list(result, emoji_to_wire);
        Ok(api::Z_GetEmojiListReturns { a, b })
    }

    /// Port of `PluginAPI.GetEmojiImage` (app/plugin_api.go:1091): the bytes and the decoder's
    /// format name (`png`, `gif`, …), with none of `GetEmoji`'s gates.
    pub(super) async fn channels_get_emoji_image(
        &self,
        args: api::Z_GetEmojiImageArgs,
    ) -> Result<api::Z_GetEmojiImageReturns, NotImplemented> {
        let answer = match self.app.get_emoji_image(&args.a).await {
            Ok((bytes, format)) => api::Z_GetEmojiImageReturns {
                a: bytes,
                b: format.to_owned(),
                c: None,
            },
            Err(PrepareError::App(err)) => api::Z_GetEmojiImageReturns {
                a: Vec::new(),
                b: String::new(),
                c: self.wire(err),
            },
            Err(PrepareError::Unreproducible(why)) => {
                return Err(self.not_implemented("GetEmojiImage", why));
            }
        };
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nil_search_parameter_is_gos_defaults() {
        assert_eq!(
            search_for_user(&wire::SearchParameter::default()),
            SearchForUser {
                terms: String::new(),
                time_zone_offset: 0,
                is_or_search: false,
                page: 0,
                per_page: 100,
                include_deleted_channels: false,
            }
        );
        let set = wire::SearchParameter {
            terms: Some("words".into()),
            is_or_search: Some(true),
            time_zone_offset: Some(-19_800),
            page: Some(2),
            per_page: Some(7),
            include_deleted_channels: Some(true),
        };
        assert_eq!(
            search_for_user(&set),
            SearchForUser {
                terms: "words".into(),
                time_zone_offset: -19_800,
                is_or_search: true,
                page: 2,
                per_page: 7,
                include_deleted_channels: true,
            }
        );
    }

    #[test]
    fn a_team_search_is_answered_only_when_it_reads_nothing() {
        let term = |t: &str| wire::SearchParams {
            terms: t.into(),
            ..wire::SearchParams::default()
        };
        assert_eq!(in_team_search(false, &[term("x")]), InTeamSearch::Disabled);
        assert_eq!(in_team_search(false, &[]), InTeamSearch::Disabled);
        assert_eq!(in_team_search(true, &[]), InTeamSearch::Empty);
        assert_eq!(
            in_team_search(true, &[term("*"), term("*")]),
            InTeamSearch::Empty
        );
        assert_eq!(
            in_team_search(true, &[term("*"), term("x")]),
            InTeamSearch::Unported
        );
        assert_eq!(in_team_search(true, &[term("")]), InTeamSearch::Unported);
    }

    #[test]
    fn channel_stats_counts_the_members_twice() {
        let stats = channel_stats("c", 3, 3);
        assert_eq!(
            (
                stats.member_count,
                stats.guest_count,
                stats.pinned_post_count
            ),
            (3, 3, 0)
        );
        assert_eq!(stats.channel_id, "c");
    }

    #[test]
    fn the_space_refusal_is_gos() {
        let space = space_notify_props_error();
        assert_eq!(
            (space.where_.as_str(), space.id.as_str(), space.status_code),
            (
                "PluginAPI.rejectSpaceChannel",
                "plugin_api.channel.space_notify_props.app_error",
                400
            )
        );
    }

    #[test]
    fn a_category_crosses_both_ways_and_an_empty_list_is_nil() {
        let category = SidebarCategoryWithChannels {
            category: SidebarCategory {
                id: "custom_x".into(),
                user_id: "u".into(),
                team_id: "t".into(),
                sort_order: 30,
                sorting: "alpha".into(),
                category_type: "custom".into(),
                display_name: "Mine".into(),
                muted: true,
                collapsed: true,
            },
            channel_ids: Some(vec!["c1".into(), "c2".into()]),
        };
        let wire = sidebar_category_to_wire(&category);
        assert_eq!(wire.sidebar_category.r#type, "custom");
        assert_eq!(sidebar_category_from_wire(&wire), category);

        let empty = SidebarCategoryWithChannels {
            channel_ids: Some(Vec::new()),
            ..category
        };
        assert_eq!(
            sidebar_category_from_wire(&sidebar_category_to_wire(&empty)).channel_ids,
            None,
            "gob sends an empty slice as nil"
        );
    }

    #[test]
    fn an_emoji_crosses_whole() {
        let emoji = Emoji {
            id: "e".into(),
            create_at: 1,
            update_at: 2,
            delete_at: 3,
            creator_id: "u".into(),
            name: "party".into(),
        };
        let wire = emoji_to_wire(&emoji);
        assert_eq!(
            (
                wire.id.as_str(),
                wire.create_at,
                wire.update_at,
                wire.delete_at,
                wire.creator_id.as_str(),
                wire.name.as_str()
            ),
            ("e", 1, 2, 3, "u", "party")
        );
    }
}
