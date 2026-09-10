//! Port of the write half of `server/channels/app/post.go` — `UpdatePost` (:851) and `PatchPost`
//! (:1288), which between them are the app layer behind `PUT /posts/{id}`,
//! `PUT /posts/{id}/patch`, `POST /posts/{id}/pin` and `POST /posts/{id}/unpin`.
//!
//! Kept out of [`crate::post`], which is 1,500 lines of read path, because nothing here shares a
//! helper with it beyond `PrepareError`.
//!
//! # An edit is a row *and* a row
//!
//! `SqlPostStore.Update` rewrites the live post and **inserts the old version as a new row** with
//! `OriginalId` set and `DeleteAt` stamped — see [`mm_store::post_store::PostStore::update`]. So
//! every route in this module adds an entry to the post's edit history, including a pin, which
//! changes nothing else a reader can see.
//!
//! # What this refuses, and why each refusal is narrow
//!
//! Go's update path fans out into machinery this server does not have. Each of the following is a
//! [`PrepareError::Unreproducible`] rather than a half-answer, and each is gated on a shape a
//! request has to *carry* rather than on the route:
//!
//! | Go branch | Refused when |
//! |---|---|
//! | `PostWithProxyRemovedFromImageURLs` (post.go:2466) | `ImageProxySettings.Enable` is on |
//! | `processPostFileChanges` (post_file_change.go:12) | the edit changes the file id set |
//! | `FillInPostProps`' channel mentions (post.go:568) | the post carries a `~channel` mention |
//! | `FillInPostProps`' group-mention prop (:632) | the message holds an `@` mention **and** the installation is licensed |
//! | `FillInPostProps`' AI-generated lookup (:637) | `ai_generated_by` is set |
//! | `RefreshInteractiveActionsOnPost` (post_interactive_blocks.go:733) | the post carries interactive content |
//! | the `PostTypeCard` ownership skip (api4/post.go:1152) | the post is a card |
//! | `AutoTranslation().Translate` (:1030) | never — it needs an enterprise licence; see the parity note below |
//!
//! # The auto-translation gap
//!
//! `a.AutoTranslation()` is an enterprise interface, nil without a licence, and its `Translate`
//! runs on **every** edit whose channel has the feature on. Nothing here reproduces it and
//! nothing here can detect it either — the feature's per-channel state lives in a table this port
//! does not read. On an unlicensed installation the whole block is dead, which is the only reason
//! this is a note and not a refusal.

use mm_model::channel::Channel;
use mm_model::permission::PERMISSION_USE_CHANNEL_MENTIONS;
use mm_model::post::{
    AllStringsOptions, POST_PROPS_ADAPTIVE_CARDS, POST_PROPS_AI_GENERATED_BY_USER_ID,
    POST_PROPS_BLOCK_KIT_BLOCKS, POST_PROPS_CHANNEL_MENTIONS, POST_PROPS_MM_BLOCKS,
    POST_PROPS_MM_BLOCKS_ACTIONS, POST_TYPE_BURN_ON_READ, POST_TYPE_CARD, Post, PostPatch,
};
use mm_model::session::Session;
use mm_model::utils::{AppError, get_millis, parse_hashtags};
use mm_model::websocket_message::{WEBSOCKET_EVENT_POST_EDITED, WebSocketEvent};
use mm_store::PostStore;

use crate::App;
use crate::channel::RestrictedDm;
use crate::license::LicenseState;
use crate::post::{PrepareError, PreparePostForClientOpts};

impl App {
    /// Port of `app.App.PatchPost` (app/post.go:1288).
    ///
    /// # It re-reads the post it is about to patch, and so does its caller
    ///
    /// `GetSinglePost` here, and `GetSinglePost` again in `postPatchChecks` and in
    /// `saveIsPinnedPost` before that — three reads of the same row per request. Reproduced,
    /// because the third read is the one whose `DeleteAt` filter decides the status code: a post
    /// deleted between the handler's read and this one answers **404 `app.post.get.app_error`**,
    /// not the handler's 403.
    ///
    /// # The mention-highlight gate asks about the **author**, not the caller
    ///
    /// `HasPermissionToChannel(post.UserId, …, use_channel_mentions)` — so an admin patching
    /// somebody else's post has the *author's* rights consulted, and a patch that adds `@channel`
    /// to a post written by a user without the permission gets `mentionHighlightDisabled` set
    /// even though the admin holds it.
    #[tracing::instrument(skip(self, patch, session), fields(post_id = %post_id))]
    pub async fn patch_post(
        &self,
        post_id: &str,
        patch: &PostPatch,
        session: &Session,
    ) -> Result<(Post, bool), PrepareError> {
        let mut post = self.get_single_post(post_id, false).await?;

        // Note the id: `patch_post`, not `update_post`. The comment above it in Go
        // ("only allow to update the pinned status…") describes an intention the code does not
        // carry out — every patch of a burn-on-read post is refused, pin included.
        if post.post_type == POST_TYPE_BURN_ON_READ {
            return Err(app_error_400(
                "PatchPost",
                "api.post.patch_post.can_not_update_burn_on_read_post.error",
            ));
        }

        let channel = self.get_channel(&post.channel_id).await?;

        if channel.delete_at != 0 {
            return Err(app_error_400(
                "PatchPost",
                "api.post.patch_post.can_not_update_post_in_deleted.error",
            ));
        }

        match self.check_if_channel_is_restricted_dm(&channel).await? {
            RestrictedDm::No => {}
            RestrictedDm::Yes => {
                return Err(app_error_400(
                    "PatchPost",
                    "api.post.patch_post.can_not_update_post_in_restricted_dm.error",
                ));
            }
            RestrictedDm::Undecidable => {
                return Err(PrepareError::Unreproducible(
                    "a bot's exemption from DM restrictions is a plugin decision",
                ));
            }
        }

        let mut patch = patch.clone();
        let (author_may_mention, _) = self
            .has_permission_to_channel(
                &post.user_id,
                &post.channel_id,
                &PERMISSION_USE_CHANNEL_MENTIONS,
            )
            .await;
        if !author_may_mention {
            patch.disable_mention_highlights();
        }

        post.patch(&patch);

        self.update_post(&post, session).await
    }

    /// Port of `app.App.UpdatePost` (app/post.go:851) for `UpdatePostOptions{SafeUpdate: false}`,
    /// which is what every REST route that reaches it passes.
    ///
    /// `SafeUpdate: true` is Shared Channels' sync path and `IsRestorePost` is
    /// `restorePostVersion`; neither is ported, and modelling their flags here would add two
    /// branches with no caller. `AllowMmBlocksActionsUpdate` is likewise false on both REST paths.
    ///
    /// # The two dead branches
    ///
    /// Go tests `oldPost == nil` (400 `api.post.update_post.find.app_error`) and
    /// `oldPost.DeleteAt != 0` (400 `api.post.update_post.permissions_details.app_error`).
    /// Neither is reachable through the store it reads: `SqlPostStore.Get` keys the returned map
    /// by the id it was given and filters `p.DeleteAt = 0`, so a missing *or* deleted post is
    /// `ErrNotFound` — **404 `app.post.get.app_error`** — long before either test. They are named
    /// here rather than ported because a reader diffing the two files will look for them.
    ///
    /// # The lookup is `GetSingle`, not `Get`
    ///
    /// Go calls `Store.Post().Get(id, GetPostsOptions{}, "", sanitizeOptions)`, which fetches the
    /// **whole thread** and then reads one entry out of the map. Every other post it loaded is
    /// discarded unread, and the two queries agree on the row and on `ReplyCount` — same
    /// `CASE WHEN RootId = ''` subquery, same `DeleteAt = 0` filter. So this reads the one post,
    /// and the difference is a query Go runs for nothing.
    #[tracing::instrument(skip(self, received, session), fields(post_id = %received.id, forwarded))]
    pub async fn update_post(
        &self,
        received: &Post,
        session: &Session,
    ) -> Result<(Post, bool), PrepareError> {
        let mut received = received.clone();
        received.sanitize_props();

        let old_post = self
            .store()
            .post()
            .get_single(&received.id, false)
            .await
            .map_err(|err| {
                let status = if err.is_invalid_input() {
                    400
                } else if err.is_not_found() {
                    404
                } else {
                    tracing::error!(error = %err, "post lookup failed");
                    500
                };
                PrepareError::App(AppError::boxed(
                    "UpdatePost",
                    "app.post.get.app_error",
                    None,
                    String::new(),
                    status,
                ))
            })?;

        if old_post.post_type == POST_TYPE_BURN_ON_READ {
            return Err(app_error_400(
                "UpdatePost",
                "api.post.update_post.burn_on_read.app_error",
            ));
        }

        // The detail carries `id=<post id>`, which the api boundary strips unless
        // `EnableDeveloper` is on — so it reaches the log and not the client.
        if old_post.is_system_message() {
            return Err(PrepareError::App(AppError::boxed(
                "UpdatePost",
                "api.post.update_post.system_message.app_error",
                None,
                format!("id={}", received.id),
                400,
            )));
        }

        let channel = self.get_channel(&old_post.channel_id).await?;

        if channel.delete_at != 0 {
            return Err(app_error_400(
                "UpdatePost",
                "api.post.update_post.can_not_update_post_in_deleted.error",
            ));
        }

        match self.check_if_channel_is_restricted_dm(&channel).await? {
            RestrictedDm::No => {}
            RestrictedDm::Yes => {
                return Err(app_error_400(
                    "UpdatePost",
                    "api.post.update_post.can_not_update_post_in_restricted_dm.error",
                ));
            }
            RestrictedDm::Undecidable => {
                return Err(PrepareError::Unreproducible(
                    "a bot's exemption from DM restrictions is a plugin decision",
                ));
            }
        }

        // The card branches in `updatePost` and `postPatchChecks` skip the ownership check for a
        // collaborative card, and `FeatureFlags.IntegratedBoards` is not in the configuration
        // document Go persists, so its value here would be a guess either way.
        if old_post.post_type == POST_TYPE_CARD {
            return Err(PrepareError::Unreproducible(
                "a card post's ownership checks turn on FeatureFlags.IntegratedBoards",
            ));
        }

        let mut new_post = old_post.clone();

        // `PostWithProxyRemovedFromImageURLs` runs on the caller's side of this function and is
        // the identity with the proxy off — but with it on it rewrites the message before the
        // comparison below, so the proxy decides whether an edit is a message change at all.
        if self.config().image_proxy_enable {
            return Err(PrepareError::Unreproducible("image proxy is enabled"));
        }

        if new_post.message != received.message {
            new_post.message.clone_from(&received.message);
            new_post.edit_at = get_millis();
            // Only the hashtag half is kept; Go discards the plain half here too.
            (new_post.hashtags, _) = parse_hashtags(&received.message);
        }

        new_post.is_pinned = received.is_pinned;
        new_post.has_reactions = received.has_reactions;
        new_post.set_props(received.get_props().cloned());
        new_post.preserve_identity_props_from(&old_post);

        // `allowMmBlocksChange`: for a REST caller it is an integration session editing its own
        // post, and it must also be *supplying* the prop. Anything else keeps whatever the old
        // post had — an edit never silently wipes buttons, and a user access token cannot inject
        // them onto somebody else's post.
        let allow_mm_blocks_change = new_post.get_prop(POST_PROPS_MM_BLOCKS_ACTIONS).is_some()
            && session.user_id == old_post.user_id
            && session.is_integration();
        if !allow_mm_blocks_change {
            match old_post
                .get_props()
                .and_then(|props| props.get(POST_PROPS_MM_BLOCKS_ACTIONS))
            {
                // Go reads the map directly rather than through `GetProp`, so a stored JSON
                // `null` is *present* here and is carried over as a null.
                Some(old_value) => {
                    let old_value = old_value.clone();
                    new_post.add_prop(POST_PROPS_MM_BLOCKS_ACTIONS, old_value);
                }
                None => new_post.del_prop(POST_PROPS_MM_BLOCKS_ACTIONS),
            }
        }

        // `RefreshInteractiveActionsOnPost` prunes the registry to the actions the content still
        // references. With no interactive content — the only shape served here — the whole
        // function is `delete(props, mm_blocks_actions)`; with any, it needs
        // `CollectInteractiveActionIDsFromPost` and `SubsetMmBlocksActions`.
        if carries_interactive_content(&new_post) {
            return Err(PrepareError::Unreproducible(
                "pruning mm_blocks_actions needs CollectInteractiveActionIDsFromPost",
            ));
        }
        new_post.del_prop(POST_PROPS_MM_BLOCKS_ACTIONS);

        // `processPostFileChanges` compares the **received** post's ids against the old post's,
        // and attaching or detaching a file is a write to `FileInfo` this port does not do. The
        // comparison is over de-duplicated lists, as Go's is, so a client that repeats an id is
        // not treated as a change.
        let old_file_ids = deduplicated(old_post.file_ids.as_deref());
        let new_file_ids = deduplicated(received.file_ids.as_deref());
        if old_file_ids != new_file_ids {
            return Err(PrepareError::Unreproducible(
                "adding or removing a file attaches or detaches a FileInfo row",
            ));
        }
        // Go assigns `processPostFileChanges`' return, which is the *received* list — not the
        // de-duplicated one, and not the old post's.
        new_post.file_ids = received.file_ids.clone();

        // `EditAt` is bumped a second time for a file or attachment change the message did not
        // already account for. `Equals` is length-then-elements, so a nil list and an empty one
        // are equal — which is what stops a post that never had files from being marked edited.
        if new_post.edit_at == old_post.edit_at
            && (old_post.file_ids.as_deref().unwrap_or_default()
                != new_post.file_ids.as_deref().unwrap_or_default()
                || !old_post.attachments_equal(&new_post))
        {
            new_post.edit_at = get_millis();
        }

        self.fill_in_post_props(&mut new_post).await?;

        // `oldPost.RemoteId = new(*receivedUpdatedPost.RemoteId)` — it mutates the **old** post,
        // which is about to become the edit-history row, so a federated edit stamps the history
        // entry with the remote's id. `SanitizeInput` blanks `RemoteId` on both REST paths, so
        // this is reachable only from Shared Channels sync.
        let mut old_post_for_history = old_post.clone();
        if received.is_remote() {
            old_post_for_history.remote_id = received.remote_id.clone();
        }

        // `runGuardedMessageWillBeUpdated` is the identity with no plugin environment.

        // Always the incoming metadata when there is one, with `Embeds` stripped —
        // server-generated, never client-supplied. Otherwise the old post's, which the store
        // never selects and so is always absent here.
        match received.metadata.as_ref() {
            Some(metadata) => {
                let mut metadata = metadata.clone();
                metadata.embeds = Vec::new();
                new_post.metadata = Some(metadata);
            }
            None => new_post.metadata = old_post.metadata.clone(),
        }

        let saved = self
            .store()
            .post()
            .update(&new_post, &old_post_for_history)
            .await
            .map_err(|err| match err {
                // `errors.As(nErr, &appErr)` — `IsValid`'s refusal reaches the client with its own
                // id and status, not wrapped in `app.post.update.app_error`.
                mm_store::StoreError::Invalid { app_error, .. } => PrepareError::App(app_error),
                err => {
                    tracing::error!(error = %err, "post update failed");
                    PrepareError::App(AppError::boxed(
                        "UpdatePost",
                        "app.post.update.app_error",
                        None,
                        String::new(),
                        500,
                    ))
                }
            })?;

        // `MessageHasBeenUpdated` is a plugin hook; there is no plugin environment.

        let mut prepared = self
            .prepare_post_for_client_with_embeds_and_images(
                &saved,
                PreparePostForClientOpts {
                    is_edit_post: true,
                    include_priority: true,
                    ..PreparePostForClientOpts::default()
                },
            )
            .await?;

        // Nulled because the edited post is broadcast to everyone in the channel and `IsFollowing`
        // is per-recipient. It is also what the HTTP caller gets back.
        prepared.is_following = None;

        // `addPostPreviewProp` would run here and, if the post carried a permalink preview, write
        // the row a **second** time. `GetPreviewPost` reads `Metadata.Embeds` for a permalink
        // embed, and any post that could have one carries `previewed_post` — a refused prop, so
        // `prepare_post_for_client_with_embeds_and_images` above has already declined. Nothing
        // reaching this line has an embed to find.

        // `AutoTranslation().Translate` would run here on a licensed installation with the
        // feature enabled for the channel. See the module docs.

        self.publish_websocket_event_for_post(WEBSOCKET_EVENT_POST_EDITED, &prepared)
            .await;

        // **After the publish**, so the event carries the unsanitised post and the HTTP response
        // carries the sanitised one. On the shapes served here they are the same value.
        let (sanitized, is_member_for_previews) = self
            .sanitize_post_metadata_for_user(prepared, &session.user_id)
            .await?;

        tracing::Span::current().record("forwarded", false);
        Ok((sanitized, is_member_for_previews))
    }

    /// Port of `app.App.MaxPostSize` (app/post.go:2500), which is
    /// `Platform().MaxPostSize()` → `Store.Post().GetMaxPostSize()`.
    ///
    /// **Go memoises it in a `sync.Once` and this does not**, so a request that consults it pays
    /// an `information_schema` query. That is the same choice `max_draft_size` made; nothing about
    /// it is on the wire, and a cache here would be a second place for the value to be stale.
    ///
    /// A store failure is `app.post.max_post_size.app_error` at 500 — an id Go does not have,
    /// because Go cannot fail here: `determineMaxPostSize` swallows its own error. The store does
    /// the swallowing, so this arm is unreachable for the same reason.
    #[tracing::instrument(skip(self))]
    pub async fn max_post_size(&self) -> Result<usize, Box<AppError>> {
        self.store().post().max_post_size().await.map_err(|err| {
            tracing::error!(error = %err, "reading the maximum post size failed");
            AppError::boxed(
                "MaxPostSize",
                "app.post.max_post_size.app_error",
                None,
                String::new(),
                500,
            )
        })
    }

    /// Port of `app.App.FillInPostProps` (app/post.go:566) for the shapes an edit can reach.
    ///
    /// Called with a nil `channel` on this path, which is what makes the `Channel().GetForPost`
    /// lookup inside the channel-mentions branch reachable at all — and that branch is refused
    /// here, so the lookup is not needed.
    ///
    /// # What survives
    ///
    /// The `channel_mentions` **deletion**. Go's `else if post.GetProps() != nil` arm removes a
    /// stale prop whenever the post no longer mentions a channel, and a post read out of the
    /// store always has a materialised props map — so the arm is taken on every edit. It is a
    /// no-op on a post that never had the prop, and the prop itself is refused, so the only way to
    /// observe it is a mutation that removes it.
    async fn fill_in_post_props(&self, post: &mut Post) -> Result<(), PrepareError> {
        // `ChannelMentionsAllWithOptions` reads the message *and* the attachments and interactive
        // payloads. `omit_interactive_blocks` is `!FeatureFlags.MmBlocksEnabled`, and that flag
        // defaults to true, so the blocks are walked.
        let channel_mentions = post.channel_mentions_all_with_options(AllStringsOptions {
            omit_interactive_blocks: false,
        });
        if !channel_mentions.is_empty() {
            return Err(PrepareError::Unreproducible(
                "a ~channel mention resolves channels and teams into a prop",
            ));
        }
        if post.get_props().is_some() {
            post.del_prop(POST_PROPS_CHANNEL_MENTIONS);
        }

        // `a.Srv().License() != nil && *License.Features.LDAPGroups && matched` — the feature bit
        // lives in the signed licence body, which this server never parses. Unlicensed, the
        // conjunction is false whatever the message says, so the licence is only consulted when
        // there is an `@` for it to matter to.
        if has_at_mention(&post.message) && self.license_state().await? == LicenseState::Licensed {
            return Err(PrepareError::Unreproducible(
                "the group-mention prop turns on the licence's LDAPGroups feature bit",
            ));
        }

        if post.get_prop(POST_PROPS_AI_GENERATED_BY_USER_ID).is_some() {
            return Err(PrepareError::Unreproducible(
                "ai_generated_by is resolved to a username through a user lookup",
            ));
        }

        // The burn-on-read arm is unreachable: `UpdatePost` refuses that post type outright.
        Ok(())
    }

    /// Port of `app.App.publishWebsocketEventForPost` (app/post.go:1097) for the shapes an edit
    /// and a delete can reach.
    ///
    /// # Four of its five stages are inert here
    ///
    /// The burn-on-read content blanking needs that post type (refused). The permalink hook needs
    /// `previewed_post` and the channel-mentions hook needs `channel_mentions` — both refused
    /// props, so `removePermalinkMetadataFromPost` and the `DelProp` have nothing to remove. The
    /// ABAC files hook needs `AccessControlSettings.EnableAttributeBasedAccessControl`, an
    /// enterprise setting, and `FeatureFlags.PermissionPolicies`. What is left is the serialisation
    /// and the publish.
    ///
    /// # The event names no user and omits no connection
    ///
    /// `NewWebSocketEvent(event, "", post.ChannelId, "", nil, "")` — channel-scoped, with an empty
    /// `omit_connection_id`. So the client that made the edit **is** told about it, unlike a draft
    /// save; a port that helpfully threaded the `Connection-Id` header through here would silently
    /// stop the editing tab from seeing its own edit.
    async fn publish_websocket_event_for_post(&self, event: &str, post: &Post) {
        let mut message = WebSocketEvent::new(event, "", &post.channel_id, "", None, "");
        match post.to_json() {
            Ok(json) => message.add("post", serde_json::Value::String(json)),
            Err(err) => {
                // Go answers 500 `app.post.marshal.app_error` here. A `Post` cannot fail to
                // serialise — every field is a JSON-representable owned value — so the branch is
                // logged rather than propagated, and the caller keeps one error type fewer.
                tracing::error!(error = %err, post_id = %post.id, "Error in marshalling post to JSON");
                return;
            }
        }
        self.publish(message).await;
    }
}

/// `RemoveDuplicateStrings` over an optional list, for the set comparison
/// `utils.FindExclusives` makes.
///
/// Go's `RemoveDuplicateStrings` **sorts** as well as de-duplicating, which is what makes this a
/// set comparison rather than a sequence one: `["a","b"]` and `["b","a"]` are not a file change.
fn deduplicated(ids: Option<&[String]>) -> Vec<String> {
    let mut ids = ids.unwrap_or_default().to_vec();
    mm_model::utils::remove_duplicate_strings(&mut ids);
    ids
}

/// Whether the post carries any of the three interactive dialects, which is what
/// `CollectInteractiveActionIDsFromPost` walks along with `mmaction://` links in the message.
///
/// All three props are in [`crate::post::REFUSED_PROPS`] as well, so a post carrying one is
/// forwarded either way — but that refusal happens *after* the store write, and this one has to
/// happen before it.
fn carries_interactive_content(post: &Post) -> bool {
    post.message.contains("mmaction://")
        || post.get_props().is_some_and(|props| {
            [
                POST_PROPS_MM_BLOCKS,
                POST_PROPS_BLOCK_KIT_BLOCKS,
                POST_PROPS_ADAPTIVE_CARDS,
            ]
            .iter()
            .any(|key| props.contains_key(*key))
        })
}

/// Go's `atMentionPattern` = `\B@` (app/post.go:36), which is "an `@` **not** preceded by a word
/// character".
///
/// RE2's `\b` is ASCII, so `_`, the digits and the two ASCII letter ranges are the whole word
/// class — a `@` after `é` matches here and in Go alike. At the start of the string both sides are
/// non-word, which is not a boundary, so a message beginning with `@here` matches.
fn has_at_mention(message: &str) -> bool {
    message.char_indices().any(|(index, ch)| {
        ch == '@'
            && !message[..index]
                .chars()
                .next_back()
                .is_some_and(|prev| prev.is_ascii_alphanumeric() || prev == '_')
    })
}

/// The four `400`s this module raises, which differ only in their id.
fn app_error_400(where_: &'static str, id: &'static str) -> PrepareError {
    PrepareError::App(AppError::boxed(where_, id, None, String::new(), 400))
}

/// `postEditTimeLimitExpired` (api4/post.go:1052), which three of the four routes consult.
///
/// Lifted here from the handler so it can be tested without a request: it is pure, and the branch
/// it guards is a 400 nothing on a default-configured server can reach.
///
/// **`-1` is checked for explicitly and is not a comparison.** A limit of `0` means every post is
/// already past its edit window, so the two values that both look like "off" behave in opposite
/// ways. The unit is **seconds**; the deadline is `CreateAt + limit*1000`.
pub fn post_edit_time_limit_expired(post_edit_time_limit: i64, post: &Post) -> bool {
    if post_edit_time_limit == -1 {
        return false;
    }
    get_millis() > post.create_at + post_edit_time_limit * 1000
}

/// `channel.DeleteAt != 0` on a channel this module fetched, named so a reader of the handlers
/// sees which check is which.
pub fn channel_is_archived(channel: &Channel) -> bool {
    channel.delete_at != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post_at(create_at: i64) -> Post {
        Post {
            create_at,
            ..Post::default()
        }
    }

    /// `-1` is off. `0` is not — it expires every post immediately, which is the opposite.
    #[test]
    fn the_edit_time_limit_treats_minus_one_and_zero_oppositely() {
        let post = post_at(get_millis());
        assert!(!post_edit_time_limit_expired(-1, &post));
        assert!(post_edit_time_limit_expired(0, &post));
    }

    /// The unit is seconds, so a five-second limit still admits a post made a second ago.
    #[test]
    fn the_edit_time_limit_is_seconds_not_milliseconds() {
        let post = post_at(get_millis() - 1_000);
        assert!(!post_edit_time_limit_expired(5, &post));
        let older = post_at(get_millis() - 6_000);
        assert!(post_edit_time_limit_expired(5, &older));
    }

    /// `\B@`: an `@` at the start of the string matches, one after a letter, digit or underscore
    /// does not, and one after punctuation or a non-ASCII letter does.
    #[test]
    fn at_mention_matches_gos_non_word_boundary() {
        assert!(has_at_mention("@here we go"));
        assert!(has_at_mention("hey @channel"));
        assert!(has_at_mention("(@all)"));
        assert!(has_at_mention("é@all"));
        assert!(!has_at_mention("someone@example.com"));
        assert!(!has_at_mention("a1_@nope"));
        assert!(!has_at_mention("no mentions here"));
        assert!(!has_at_mention(""));
    }

    #[test]
    fn deduplication_makes_the_file_comparison_a_set_comparison() {
        let a = vec!["b".to_owned(), "a".to_owned()];
        let b = vec!["a".to_owned(), "b".to_owned(), "a".to_owned()];
        assert_eq!(deduplicated(Some(&a)), deduplicated(Some(&b)));
        assert_eq!(deduplicated(None), Vec::<String>::new());
        assert_eq!(deduplicated(Some(&[])), Vec::<String>::new());
    }

    #[test]
    fn interactive_content_is_detected_by_prop_or_by_link() {
        let mut post = Post::default();
        assert!(!carries_interactive_content(&post));
        post.message = "click mmaction://do-it".to_owned();
        assert!(carries_interactive_content(&post));

        let mut post = Post::default();
        post.add_prop(POST_PROPS_MM_BLOCKS, serde_json::json!([]));
        assert!(carries_interactive_content(&post));
    }
}
