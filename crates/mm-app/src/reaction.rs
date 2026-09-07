//! Port of the read side of `server/channels/app/reaction.go`.

use std::collections::BTreeMap;

use mm_model::post::Post;
use mm_model::reaction::Reaction;
use mm_model::utils::{AppError, AppResult};
use mm_store::PostStore;
use mm_store::reaction_store::ReactionStore;

use crate::App;

impl App {
    /// Port of `app.App.GetReactionsForPost` (app/reaction.go:121).
    ///
    /// One store call and one error id. There is **no not-found branch**: a post that does not
    /// exist, or has never been reacted to, is a successful read of zero rows — the 500 is
    /// reserved for a genuine query failure. So `getReactions` cannot 404, which is the
    /// difference between it and every other `/posts/{post_id}/…` read.
    ///
    /// # The empty result is `null` on the wire, not `[]`
    ///
    /// Go declares `var reactions []*model.Reaction` and `SelectBuilder` appends into it, so
    /// zero rows leave the slice **nil**; `json.Marshal` renders a nil slice as `null`. The
    /// handler marshals the app layer's return value directly, with no `if len == 0` in
    /// between, so a post with no reactions answers the four bytes `null`. Preserved by
    /// returning `Vec<Reaction>` here and letting `mm_api::reactions` decide the bytes — see
    /// that module for why the decision lives there and not in this signature.
    #[tracing::instrument(skip(self), fields(post_id = %post_id))]
    pub async fn get_reactions_for_post(&self, post_id: &str) -> AppResult<Vec<Reaction>> {
        self.store()
            .reaction()
            .get_for_post(post_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "reaction lookup failed");
                AppError::boxed(
                    "GetReactionsForPost",
                    "app.reaction.get_for_post.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetBulkReactionsForPosts` (app/reaction.go:129).
    ///
    /// One store call, one grouping pass, and `populateEmptyReactions` (app/reaction.go:148) —
    /// which is why the answer names **every** requested post, including ids that match no post
    /// at all. The handler above it validates no id, so `["abc"]` is a legal request that
    /// answers `{"abc": []}`.
    ///
    /// # `[]` here, `null` next door
    ///
    /// [`Self::get_reactions_for_post`] answers the four bytes `null` for a post with no
    /// reactions, because Go's slice is nil and nothing fills it in. This route's empty value is
    /// `[]`, because `populateEmptyReactions` assigns a literal `[]*model.Reaction{}`. Two
    /// routes, the same absence, two different bytes — a `BTreeMap<String, Vec<Reaction>>`
    /// serialised straight through gives this one the right ones.
    ///
    /// # The map is ordered, and that is wire surface
    ///
    /// `encoding/json` sorts map keys bytewise when it marshals, so Go's response object is in
    /// ascending id order regardless of the request's. `BTreeMap<String>` orders bytewise too,
    /// so serialising it reproduces that without a sort step. (The request list is already
    /// sorted by `SortedArrayFromJSON`, so the two agree twice over.)
    ///
    /// # An empty id list is a 500
    ///
    /// The store refuses it, and that refusal is not a not-found — so it lands in the same 500
    /// as a driver failure, with the same error id. See
    /// `mm_store::reaction_store::SqlReactionStore::bulk_get_for_posts` for why Go fails there.
    #[tracing::instrument(skip(self, post_ids), fields(asked = post_ids.len()))]
    pub async fn get_bulk_reactions_for_posts(
        &self,
        post_ids: &[String],
    ) -> AppResult<BTreeMap<String, Vec<Reaction>>> {
        let all = self
            .store()
            .reaction()
            .bulk_get_for_posts(post_ids)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "bulk reaction lookup failed");
                AppError::boxed(
                    "GetBulkReactionsForPosts",
                    "app.reaction.bulk_get_for_post_ids.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let mut grouped: BTreeMap<String, Vec<Reaction>> = BTreeMap::new();
        for reaction in all {
            // The id is needed twice — once as the key, once inside the value, where
            // `post_id` is a wire field this route echoes back. Taking it out of the
            // `Reaction` would blank it on the response, so the copy is the cheap half.
            grouped
                .entry(reaction.post_id.clone())
                .or_default()
                .push(reaction);
        }

        // `populateEmptyReactions` (app/reaction.go:148). A post the query matched nothing for
        // still gets a key, and one the query answered for is left alone.
        for post_id in post_ids {
            grouped.entry(post_id.clone()).or_default();
        }

        Ok(grouped)
    }
}

/// What [`App::save_reaction_for_post`] or [`App::delete_reaction_for_post`] could not decide.
///
/// Both write paths consult two things this server does not have. Rather than approximate either,
/// the app layer says so and `mm_api::reactions` forwards the whole request to Go, which is the
/// standing "reproduce what we can measure, forward what we cannot" decision applied at the
/// granularity of one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undecidable {
    /// The post is `burn_on_read`, so Go consults a `ReadReceipts` row before allowing the
    /// reaction. That store is not ported.
    BurnOnReadPost,
    /// The post really is a persistent-notification post, so Go calls
    /// `ResolvePersistentNotification` after the write — which can *fail the whole request* — and
    /// resolving it needs the mention parser and a delete of the notification row. Unported.
    ///
    /// Reached only when a live `PersistentNotifications` row exists, the reactor is not the
    /// post's author, and the feature is on. The first three lines of Go's function decide every
    /// other case, and they are ported.
    PersistentNotifications,
    /// A bot is an active member of a restricted DM and its exemption is a plugin decision.
    BotInRestrictedDm,
}

/// The outcome of a reaction write: the answer, or the reason this server declined to give one.
#[derive(Debug)]
pub enum ReactionWrite<T> {
    Done(T),
    Forward(Undecidable),
}

impl App {
    /// Port of `app.App.SaveReactionForPost` (app/reaction.go:19).
    ///
    /// # The order of the checks is the answer
    ///
    /// Seven gates before the insert, each with its own id and status, and three of them can fire
    /// on the same request:
    ///
    /// 1. the post must exist (404 from `GetSinglePost`);
    /// 2. burn-on-read posts need a read receipt — **forwarded**, see [`Undecidable`];
    /// 3. the emoji must be a system emoji *or* a custom one (404 `api.emoji.get.app_error`);
    /// 4. the unique-emoji limit, **skipped entirely when the emoji is already on the post** —
    ///    so the 51st *distinct* emoji is refused while the 51st reaction with an existing emoji
    ///    is not;
    /// 5. the channel must exist;
    /// 6. a restricted DM is a **400**, not a 403;
    /// 7. an archived channel is a **403**, not a 400.
    ///
    /// Six and seven differ in status and are adjacent in the Go source, which is exactly the
    /// kind of pair a port swaps.
    ///
    /// # `channel_id` is written from the post, not from the request
    ///
    /// Go sets `reaction.ChannelId = post.ChannelId` right before the insert — "pre-populating
    /// the channelID to save a DB call in store". A client that sends a `channel_id` therefore
    /// has it overwritten, and the value on the wire in the response is the post's.
    #[tracing::instrument(skip(self, reaction), fields(post_id = %reaction.post_id, emoji = %reaction.emoji_name))]
    pub async fn save_reaction_for_post(
        &self,
        reaction: &Reaction,
    ) -> AppResult<ReactionWrite<Reaction>> {
        let mut reaction = reaction.clone();
        // `go_to_lower`, not `str::to_lowercase` — see the note on the delete path below.
        reaction.emoji_name = mm_model::utils::go_to_lower(&reaction.emoji_name);

        let post = self.get_single_post(&reaction.post_id, false).await?;

        if post.post_type == mm_model::post::POST_TYPE_BURN_ON_READ
            && post.user_id != reaction.user_id
        {
            return Ok(ReactionWrite::Forward(Undecidable::BurnOnReadPost));
        }

        // `GetSystemEmojiId` first, so a name that collides with a system emoji never reaches the
        // custom-emoji lookup. The custom lookup's error is returned **unchanged** — Go does not
        // wrap it — so an unknown emoji answers `api.emoji.get.app_error` at 404 rather than a
        // reaction-specific id.
        if mm_model::emoji::get_system_emoji_id(&reaction.emoji_name).is_none() {
            self.get_emoji_by_name(&reaction.emoji_name).await?;
        }

        let existing = self
            .store()
            .reaction()
            .exists_on_post(&reaction.post_id, &reaction.emoji_name)
            .await
            .map_err(|err| save_store_error("existing-reaction check", err))?;

        if !existing {
            let count = self
                .store()
                .reaction()
                .get_unique_count_for_post(&reaction.post_id)
                .await
                .map_err(|err| save_store_error("unique-reaction count", err))?;

            if count >= self.config().unique_emoji_reaction_limit_per_post {
                return Err(AppError::boxed(
                    "SaveReactionForPost",
                    "app.reaction.save.save.too_many_reactions",
                    None,
                    String::new(),
                    400,
                ));
            }
        }

        let channel = self.get_channel(&post.channel_id).await?;

        match self.check_if_channel_is_restricted_dm(&channel).await? {
            crate::channel::RestrictedDm::No => {}
            crate::channel::RestrictedDm::Yes => {
                return Err(AppError::boxed(
                    "SaveReactionForPost",
                    "api.reaction.save.restricted_dm.error",
                    None,
                    String::new(),
                    400,
                ));
            }
            crate::channel::RestrictedDm::Undecidable => {
                return Ok(ReactionWrite::Forward(Undecidable::BotInRestrictedDm));
            }
        }

        if channel.delete_at > 0 {
            return Err(AppError::boxed(
                "SaveReactionForPost",
                "api.reaction.save.archived_channel.app_error",
                None,
                String::new(),
                403,
            ));
        }

        // Go runs `ResolvePersistentNotification` **after** the insert, and returns its error —
        // which would leave the reaction written and the request failed. That ordering cannot be
        // reproduced by declining afterwards, so the decision is taken *before* anything is
        // written and the whole request is forwarded, letting Go do both halves.
        //
        // Most requests are decided here without forwarding, because Go's own function gives up
        // on its first three lines: the post's author reacting to their own post is exempt, the
        // feature can be off, and above all **the post has to be a persistent-notification post**,
        // which almost none are. Only a live row makes this undecidable.
        if post.root_id.is_empty()
            && post.user_id != reaction.user_id
            && self.is_persistent_notifications_enabled()
            && self
                .store()
                .post()
                .has_persistent_notification(&post.id)
                .await
                .map_err(|err| save_store_error("persistent notification lookup", err))?
        {
            return Ok(ReactionWrite::Forward(Undecidable::PersistentNotifications));
        }

        reaction.channel_id = post.channel_id.clone();
        reaction.pre_save();
        reaction.is_valid()?;

        self.store()
            .reaction()
            .save(&reaction)
            .await
            .map_err(|err| save_store_error("reaction save", err))?;

        self.send_reaction_event(
            mm_model::websocket_message::WEBSOCKET_EVENT_REACTION_ADDED,
            &reaction,
            &post,
        )
        .await;

        Ok(ReactionWrite::Done(reaction))
    }

    /// Port of `app.App.DeleteReactionForPost` (app/reaction.go:157).
    ///
    /// The same channel gates as the save path, in the same order and with the same two statuses
    /// — but **none** of the emoji, limit or burn-on-read checks. Removing a reaction that was
    /// never there is not an error: the UPDATE matches no rows and Go reports success.
    #[tracing::instrument(skip(self, reaction), fields(post_id = %reaction.post_id, emoji = %reaction.emoji_name))]
    pub async fn delete_reaction_for_post(
        &self,
        reaction: &Reaction,
    ) -> AppResult<ReactionWrite<()>> {
        let mut reaction = reaction.clone();
        // **`go_to_lower`, not `str::to_lowercase`.** Go's `strings.ToLower` is the *simple*
        // per-rune case mapping; Rust's is the full Unicode one, and they disagree on inputs a
        // client can send: `İ` (U+0130) is one byte `i` to Go and three bytes `i` + U+0307 to
        // Rust. The emoji name is compared against stored names and measured against a 64-byte
        // cap, so the difference decides both which row is found and whether the request is
        // refused. Caught by a surviving mutation; `go_to_lower`'s own doc comment names this
        // exact case.
        reaction.emoji_name = mm_model::utils::go_to_lower(&reaction.emoji_name);

        let post = self.get_single_post(&reaction.post_id, false).await?;
        let channel = self.get_channel(&post.channel_id).await?;

        match self.check_if_channel_is_restricted_dm(&channel).await? {
            crate::channel::RestrictedDm::No => {}
            crate::channel::RestrictedDm::Yes => {
                return Err(AppError::boxed(
                    "DeleteReactionForPost",
                    "api.reaction.delete.restricted_dm.error",
                    None,
                    String::new(),
                    400,
                ));
            }
            crate::channel::RestrictedDm::Undecidable => {
                return Ok(ReactionWrite::Forward(Undecidable::BotInRestrictedDm));
            }
        }

        if channel.delete_at > 0 {
            return Err(AppError::boxed(
                "DeleteReactionForPost",
                "api.reaction.delete.archived_channel.app_error",
                None,
                String::new(),
                403,
            ));
        }

        reaction.pre_update();

        // The id on this error is `app.reaction.delete_all_with_emoji_name.get_reactions.app_error`
        // — a *different* operation's id, pasted. Reproduced, because a client that branches on it
        // branches on what Go sends.
        self.store()
            .reaction()
            .delete(&reaction)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "reaction delete failed");
                AppError::boxed(
                    "DeleteReactionForPost",
                    "app.reaction.delete_all_with_emoji_name.get_reactions.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        self.send_reaction_event(
            mm_model::websocket_message::WEBSOCKET_EVENT_REACTION_REMOVED,
            &reaction,
            &post,
        )
        .await;

        Ok(ReactionWrite::Done(()))
    }

    /// Port of `app.App.sendReactionEvent` (app/reaction.go:203).
    ///
    /// **The reaction is a JSON string inside the event's `data`, not a nested object.** Go
    /// marshals it and calls `message.Add("reaction", string(reactionJSON))`, so the client
    /// decodes twice. Emitting an object here would be the same information in a shape no client
    /// parses.
    ///
    /// The broadcast carries the **channel** and nothing else, which is what makes
    /// `reaction_added` presence-scoped in the hub: a member who does not have the channel or its
    /// thread open does not receive it.
    async fn send_reaction_event(&self, event: &str, reaction: &Reaction, post: &Post) {
        let mut message = mm_model::websocket_message::WebSocketEvent::new(
            event,
            "",
            &post.channel_id,
            "",
            None,
            "",
        );
        match serde_json::to_string(reaction) {
            Ok(json) => message.add("reaction", serde_json::Value::String(json)),
            Err(err) => {
                // Go logs and carries on, publishing an event whose `data` has no `reaction` key.
                tracing::warn!(error = %err, "Failed to encode reaction to JSON");
            }
        }

        // Go additionally applies `useBurnOnReadReactionHook` for a burn-on-read post. That path
        // is forwarded before it can be reached — see `Undecidable::BurnOnReadPost`.
        self.publish(message).await;
    }

    /// Port of `app.App.IsPersistentNotificationsEnabled` (post_persistent_notification.go:430).
    ///
    /// Both halves default to **true**, so this is on for a stock server — which is why the
    /// reaction path forwards rather than treating it as an unreachable branch.
    pub fn is_persistent_notifications_enabled(&self) -> bool {
        self.config().post_priority && self.config().allow_persistent_notifications
    }
}

/// Both of `SaveReactionForPost`'s store failures share one id — `app.reaction.save.save.app_error`
/// at 500 — across three different queries.
fn save_store_error(what: &str, err: mm_store::StoreError) -> Box<AppError> {
    tracing::error!(error = %err, what, "reaction save failed");
    AppError::boxed(
        "SaveReactionForPost",
        "app.reaction.save.save.app_error",
        None,
        String::new(),
        500,
    )
}
