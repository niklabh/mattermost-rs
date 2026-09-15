//! Port of the post-search slice of `app/post.go`: `SearchPostsForUser` (post.go:2252) and the
//! helpers it reaches — `convertChannelNamesToChannelIds` (:2141), `convertUserNameToUserIds`
//! (:2153), `parseAndFetchChannelIdByNameFromInFilter` (:2057) and
//! `FilterPostsByChannelPermissions` (:2314) — plus `GetGroupChannel` (channel.go:727), which the
//! `in:` filter reaches for a group channel and which nothing else on this server calls.
//!
//! # What the search engines would have done
//!
//! Go's `SearchPostsForUser` has no engine branch of its own — Elasticsearch and Bleve hang off
//! `searchPostsInTeam`, the plugin-API entry point — but the *store's* `SearchPostsForUser` is
//! what the engines replace when one is active, and on this deployment none is: `SearchEngine`
//! is the database. So the database branch is the only branch, ported whole, and there is
//! nothing here to forward on that account.
//!
//! # Two filters that cannot remove anything here
//!
//! `filterInaccessiblePosts` returns before touching the list unless a licence carrying a
//! `PostHistory` limit is loaded, which none is (see [`crate::App::get_post_thread`]); and
//! `filterBurnOnReadPosts` looks for `burn_on_read` posts that the store's search has already
//! skipped row by row (`SqlPostStore.search`, post_store.go:2370). Neither is ported, and the
//! doc on [`crate::App::search_posts_for_user`] is where a future licence port should look.

use std::collections::{BTreeMap, HashMap};

use mm_model::channel::{
    CHANNEL_GROUP_MAX_USERS, CHANNEL_GROUP_MIN_USERS, Channel, get_group_name_from_user_ids,
};
use mm_model::post_list::{PostList, PostMap};
use mm_model::post_search_results::PostSearchResults;
use mm_model::search_params::parse_search_params;
use mm_model::utils::{AppError, AppResult, StringArray};
use mm_store::StoreError;
use mm_store::post_store::PostStore;
use mm_store::user_store::UserStore;

use crate::App;
use crate::channel_create::ChannelCreate;

/// How a search can fail short of a Go-shaped error.
#[derive(Debug, thiserror::Error)]
pub enum PostSearchError {
    /// A branch this server does not reproduce was reached; the request belongs to Go.
    #[error("post search is not reproducible here: {0}")]
    Unreproducible(&'static str),
    #[error(transparent)]
    App(#[from] Box<AppError>),
}

/// What `parseAndFetchChannelIdByNameFromInFilter` resolved an `in:` operand to.
enum InFilterChannel {
    Found(Box<Channel>),
    /// `GetOrCreateDirectChannel` declined to answer — see
    /// [`App::get_or_create_direct_channel`] for the one branch that does.
    Forward(&'static str),
}

impl App {
    /// Port of `app.App.SearchPostsForUser` (post.go:2252).
    ///
    /// Returns the results and `allPostHaveMembership` — the AND over every surviving post of
    /// "the caller is a member of that channel", which Go only records in the audit log.
    ///
    /// # The order of the first two steps is Go's and does not matter
    ///
    /// The terms are parsed before `EnablePostSearch` is read. `ParseSearchParams` has no side
    /// effects, so the 501 is the same whichever runs first — kept in Go's order so the two
    /// files read alike.
    ///
    /// # `*` is dropped, and an empty list is an empty page
    ///
    /// "Don't allow users to search for `*`": a params element whose terms are exactly `*` is
    /// skipped, and when nothing survives the answer is `MakePostSearchResults(NewPostList(),
    /// nil)` with membership `true`, before the store is consulted.
    ///
    /// # `perPage` never leaves the handler
    ///
    /// Go passes it to the store, which ignores it: the database search has no paging, so
    /// `page > 0` is empty and page 0 is up to 100 rows per params element. It is not a
    /// parameter here because nothing reads it.
    #[tracing::instrument(
        skip(self, terms),
        fields(user_id = %user_id, team_id = %team_id, is_or_search, include_deleted_channels, page)
    )]
    #[allow(clippy::too_many_arguments)]
    pub async fn search_posts_for_user(
        &self,
        terms: &str,
        user_id: &str,
        team_id: &str,
        is_or_search: bool,
        include_deleted_channels: bool,
        time_zone_offset: i64,
        page: i64,
    ) -> Result<(PostSearchResults, bool), PostSearchError> {
        let params_list = parse_search_params(terms.trim(), time_zone_offset);

        if !self.config().enable_post_search {
            return Err(AppError::boxed(
                "SearchPostsForUser",
                "store.sql_post.search.disabled",
                None,
                format!("teamId={team_id} userId={user_id}"),
                501,
            )
            .into());
        }

        let mut final_params_list = Vec::with_capacity(params_list.len());
        for mut params in params_list {
            params.or_terms = is_or_search;
            params.include_deleted_channels = include_deleted_channels;
            if params.terms != "*" {
                params.in_channels = self
                    .convert_channel_names_to_channel_ids(
                        params.in_channels,
                        user_id,
                        team_id,
                        include_deleted_channels,
                    )
                    .await?;
                params.excluded_channels = self
                    .convert_channel_names_to_channel_ids(
                        params.excluded_channels,
                        user_id,
                        team_id,
                        include_deleted_channels,
                    )
                    .await?;
                params.from_users = self.convert_user_name_to_user_ids(params.from_users).await;
                params.excluded_users = self
                    .convert_user_name_to_user_ids(params.excluded_users)
                    .await;
                final_params_list.push(params);
            }
        }

        if final_params_list.is_empty() {
            return Ok((PostSearchResults::new(Some(PostList::new()), None), true));
        }

        let mut results = self
            .store()
            .post()
            .search_posts_for_user(final_params_list, user_id, team_id, page)
            .await
            .map_err(|err| match err {
                // `errors.As(err, &appErr)` — the `IsSearchParamsListValid` failure, verbatim.
                StoreError::Invalid { app_error, .. } => app_error,
                other => {
                    tracing::error!(error = %other, "post search failed");
                    AppError::boxed(
                        "SearchPostsForUser",
                        "app.post.search.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        let all_post_have_membership = self
            .filter_posts_by_channel_permissions(results.post_list.as_mut(), user_id)
            .await?;

        Ok((results, all_post_have_membership))
    }

    /// Port of `convertChannelNamesToChannelIds` (post.go:2141).
    ///
    /// Each name is replaced in place by the id it resolves to. A name that resolves to nothing
    /// is **kept as the name** — Go logs and continues — and then matches no `Channels.Id`, so
    /// `in:nonexistent` is an empty page rather than an error.
    pub(crate) async fn convert_channel_names_to_channel_ids(
        &self,
        mut channels: StringArray,
        user_id: &str,
        team_id: &str,
        include_deleted_channels: bool,
    ) -> Result<StringArray, PostSearchError> {
        for name in channels.iter_mut() {
            match self
                .parse_and_fetch_channel_id_by_name_from_in_filter(
                    name,
                    user_id,
                    team_id,
                    include_deleted_channels,
                )
                .await
            {
                Ok(InFilterChannel::Found(channel)) => *name = channel.id,
                Ok(InFilterChannel::Forward(reason)) => {
                    return Err(PostSearchError::Unreproducible(reason));
                }
                Err(err) => {
                    tracing::warn!(error = %err, name, "error getting channel id by name from in filter");
                }
            }
        }
        Ok(channels)
    }

    /// Port of `convertUserNameToUserIds` (post.go:2153). Same contract as the channel version:
    /// a leading `@` is trimmed, an unknown username stays a username and matches no `Users.Id`.
    pub(crate) async fn convert_user_name_to_user_ids(
        &self,
        mut usernames: StringArray,
    ) -> StringArray {
        for username in usernames.iter_mut() {
            match self
                .get_user_by_username(username.trim_start_matches('@'))
                .await
            {
                Ok(user) => *username = user.id,
                Err(err) => {
                    tracing::warn!(error = %err, user_name = %username, "error getting user by username");
                }
            }
        }
        usernames
    }

    /// Port of `parseAndFetchChannelIdByNameFromInFilter` (post.go:2057).
    ///
    /// Three shapes, tried in Go's order after a leading `~` is trimmed: `@a,b` is a group
    /// channel looked up by the hash of its members; `@a` is the caller's direct channel with
    /// `a`, **created if it does not exist** — a search can open a DM, and that is Go's, not a
    /// side effect to tidy; anything else is a channel name on the team being searched. For the
    /// all-teams route `team_id` is empty, and `GetChannelByName` with an empty team matches a
    /// channel of that name on any team, which is the cross-team ambiguity Go's own `TODO`
    /// above `convertChannelNamesToChannelIds` records.
    async fn parse_and_fetch_channel_id_by_name_from_in_filter(
        &self,
        channel_name: &str,
        user_id: &str,
        team_id: &str,
        include_deleted: bool,
    ) -> AppResult<InFilterChannel> {
        let clean_channel_name = channel_name.trim_start_matches('~');

        if let Some(rest) = clean_channel_name.strip_prefix('@') {
            if rest.contains(',') {
                let usernames: Vec<String> = rest.split(',').map(str::to_owned).collect();
                let users = self.get_users_by_usernames(&usernames).await?;
                let user_ids: Vec<String> = users.into_iter().map(|user| user.id).collect();
                let channel = self.get_group_channel(&user_ids).await?;
                return Ok(InFilterChannel::Found(Box::new(channel)));
            }

            let user = self.get_user_by_username(rest).await?;
            return Ok(
                match self.get_or_create_direct_channel(user_id, &user.id).await? {
                    ChannelCreate::Created(channel) => InFilterChannel::Found(channel),
                    ChannelCreate::Forward(reason) => InFilterChannel::Forward(reason),
                },
            );
        }

        let channel = self
            .get_channel_by_name(clean_channel_name, team_id, include_deleted)
            .await?;
        Ok(InFilterChannel::Found(Box::new(channel)))
    }

    /// Port of `app.App.GetGroupChannel` (channel.go:727): the size check, every id must name a
    /// user, then the channel whose name is the members' hash — deleted or not, on no team.
    ///
    /// The first two checks are [`App::create_group_channel`]'s, but the `where` differs
    /// (`GetGroupChannel`, not `CreateGroupChannel`) and Go writes `where` on the wire, so they
    /// are spelled out rather than shared.
    #[tracing::instrument(skip_all, fields(members = user_ids.len()))]
    pub async fn get_group_channel(&self, user_ids: &[String]) -> AppResult<Channel> {
        if user_ids.len() > CHANNEL_GROUP_MAX_USERS || user_ids.len() < CHANNEL_GROUP_MIN_USERS {
            return Err(AppError::boxed(
                "GetGroupChannel",
                "api.channel.create_group.bad_size.app_error",
                None,
                String::new(),
                400,
            ));
        }

        let users = self
            .store()
            .user()
            .get_profile_by_ids(user_ids, 0)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "group member profile lookup failed");
                AppError::boxed(
                    "GetGroupChannel",
                    "app.user.get_profiles.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        if users.len() != user_ids.len() {
            return Err(AppError::boxed(
                "GetGroupChannel",
                "api.channel.create_group.bad_user.app_error",
                None,
                format!(
                    "user_ids={}",
                    serde_json::to_string(user_ids).unwrap_or_else(|_| "[]".to_owned())
                ),
                400,
            ));
        }

        self.get_channel_by_name(&get_group_name_from_user_ids(user_ids), "", true)
            .await
    }

    /// Port of `app.App.FilterPostsByChannelPermissions` (post.go:2314).
    ///
    /// Walks `Order`, keeps the posts whose channel the caller may read, and answers whether
    /// the caller is a **member** of every channel it kept. One permission check per channel,
    /// cached across the list; a channel the lookup did not return — deleted out from under the
    /// post, or the post's `ChannelId` empty — is not readable. A `GetChannels` 404 (no id
    /// resolved at all) is tolerated; any other failure is returned.
    ///
    /// Takes `Option<&mut PostList>` because the results' embedded list is a pointer in Go and
    /// an `Option` here; `nil` is "all posts have membership".
    async fn filter_posts_by_channel_permissions(
        &self,
        post_list: Option<&mut PostList>,
        user_id: &str,
    ) -> AppResult<bool> {
        let Some(list) = post_list else {
            return Ok(true);
        };
        if list.posts.as_ref().is_none_or(PostMap::is_empty) {
            // "On an empty post list, we consider all posts as having membership"
            return Ok(true);
        }

        let mut channels: BTreeMap<String, Option<Channel>> = BTreeMap::new();
        for post in list.posts.iter().flatten().map(|(_, post)| post) {
            if !post.channel_id.is_empty() {
                channels.insert(post.channel_id.clone(), None);
            }
        }

        if !channels.is_empty() {
            let channel_ids: Vec<String> = channels.keys().cloned().collect();
            match self.get_channels(&channel_ids).await {
                Ok(found) => {
                    for channel in found {
                        channels.insert(channel.id.clone(), Some(channel));
                    }
                }
                Err(err) if err.status_code == 404 => {}
                Err(err) => return Err(err),
            }
        }

        let mut channel_read_permission: HashMap<String, bool> = HashMap::new();
        let mut filtered_posts = PostMap::new();
        let mut filtered_order = Vec::new();
        let mut all_post_have_membership = true;

        // Moved out rather than cloned: both are replaced below, as Go replaces them.
        let order = list.order.take().unwrap_or_default();
        let mut posts = list.posts.take().unwrap_or_default();

        for post_id in order {
            let Some(channel_id) = posts.get(&post_id).map(|post| post.channel_id.as_str()) else {
                continue;
            };

            if !channel_read_permission.contains_key(channel_id) {
                let (allowed, is_member) = match channels.get(channel_id).and_then(Option::as_ref) {
                    Some(channel) => self.has_permission_to_read_channel(user_id, channel).await,
                    None => (false, true),
                };
                channel_read_permission.insert(channel_id.to_owned(), allowed);
                if allowed {
                    all_post_have_membership = all_post_have_membership && is_member;
                }
            }

            if channel_read_permission
                .get(channel_id)
                .copied()
                .unwrap_or(false)
            {
                if let Some(post) = posts.remove(&post_id) {
                    filtered_posts.insert(post_id.clone(), post);
                    filtered_order.push(post_id);
                }
            }
        }

        list.posts = Some(filtered_posts);
        list.order = Some(filtered_order);

        Ok(all_post_have_membership)
    }
}
