//! Port of `app.App.CreatePostAsUserWithFlags` (app/post.go:42), `app.App.CreatePost` (:173) and
//! `app.App.SendEphemeralPost` (:759) — the write behind `POST /api/v4/posts` and
//! `POST /api/v4/posts/ephemeral`.
//!
//! # `CreatePost` is 300 lines of branches on shapes; this reproduces one of them
//!
//! The single shape served is a **plain root-level message in an open or private channel**: no
//! `root_id`, no `file_ids`, no priority, no metadata, the default post type, no props that name
//! an integration or an embed, and a message with no link, no `@` and no `~`. Everything else is
//! forwarded, and [`App::refuse_create_post_shapes`] is the one function that decides — it runs
//! **before the pending-post id is claimed and before `Post().Save`**, so no forward can leave a
//! half-written row behind.
//!
//! # What the served shape still does not do, and why it is not a forward
//!
//! `handlePostEvents` ends in `SendNotifications`, whose only database write is
//! `IncrementMentionCount` for the users a post mentions. The refusals above guarantee the
//! mention set is empty — that is what the `@`, `~` and keyword-recipient checks are *for* — so
//! the fan-out has nothing to write and what remains of it is the `posted` event, reproduced in
//! [`App::publish_user_posted_event`].
//!
//! Three side effects are genuinely absent rather than refused, each following a decision this
//! project already made elsewhere:
//!
//! - **Plugin hooks** (`MessageWillBePosted`, `MessageHasBeenPosted`) — there is no plugin
//!   environment at all, [D-183]. `update_post` ships the same gap for `MessageHasBeenUpdated`.
//! - **Email and push notifications** — external, invisible to any byte comparison, and gated on
//!   `SendEmailNotifications`/`SendPushNotifications`. Recorded as [D-402].
//! - **Auto-translation** — enterprise-licensed and undetectable from the configuration document
//!   this server reads.
//!
//! Outgoing webhooks are *not* in that list: a matching outgoing webhook makes Go write a second
//! post, so [`App::refuse_create_post_shapes`] forwards any channel that has one.

use mm_model::channel::{CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE, Channel};
use mm_model::permission::PERMISSION_USE_CHANNEL_MENTIONS;
use mm_model::post::{
    POST_CUSTOM_TYPE_PREFIX, POST_PROPS_AI_GENERATED_BY_USER_ID, POST_PROPS_CURRENT_TEAM_ID,
    POST_PROPS_FROM_BOT, POST_PROPS_FROM_OAUTH_APP, POST_PROPS_FROM_PLUGIN,
    POST_PROPS_FROM_WEBHOOK, POST_PROPS_MM_BLOCKS_ACTIONS, POST_PROPS_OVERRIDE_ICON_EMOJI,
    POST_PROPS_OVERRIDE_ICON_URL, POST_PROPS_OVERRIDE_USERNAME, POST_PROPS_SILENT_NOTIFICATION,
    POST_PROPS_WEBHOOK_DISPLAY_NAME, POST_SYSTEM_MESSAGE_PREFIX, POST_TYPE_BURN_ON_READ,
    POST_TYPE_EPHEMERAL, Post,
};
use mm_model::session::Session;
use mm_model::user::User;
use mm_model::user::external::SHOW_USERNAME;
use mm_model::utils::{AppError, get_millis, new_id, parse_hashtags};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_EPHEMERAL_MESSAGE, WEBSOCKET_EVENT_POSTED, WebSocketEvent,
};

use mm_store::post_store::{GetPostThreadOptions, ThreadDirection};
use mm_store::{ChannelStore, PostStore, WebhookStore};

use crate::App;
use crate::channel::RestrictedDm;
use crate::post::{PrepareError, PreparePostForClientOpts, message_may_contain_a_link};

/// Port of `model.CreatePostFlags` (post.go:414), restricted to the two fields
/// `POST /api/v4/posts` can set.
///
/// `TriggerWebhooks` is not modelled: `CreatePostAsUserWithFlags` assigns it `true`
/// unconditionally (app/post.go:69), so on this route it is a constant. The four remaining fields
/// belong to the webhook, plugin and scheduled-post entry points, none of which reach here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CreatePostFlags {
    pub set_online: bool,
    pub silent_notification: bool,
}

/// How long a pending post id stays in the deduplication cache.
///
/// `pendingPostIDsCacheTTL = 30 * time.Second` (app/post.go:29).
const PENDING_POST_IDS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Go's `unknownPostId` — the value written when the id is claimed but the post is not yet saved.
const UNKNOWN_POST_ID: &str = "";

/// One entry of `Server.seenPendingPostIdsCache`.
#[derive(Debug, Clone)]
pub(crate) struct PendingPostEntry {
    post_id: String,
    expires_at: std::time::Instant,
}

/// The props that make this port answer differently from Go, keyed to the branch each one opens.
///
/// | prop | branch |
/// |---|---|
/// | `from_webhook` | `GetSenderName` takes `override_username`, and `CreatePostAsUser` skips `MarkChannelsAsViewed` |
/// | `from_bot`, `from_plugin` | same `MarkChannelsAsViewed` skip, and both are re-derived by Go rather than trusted |
/// | `override_username`, `override_icon_url`, `override_icon_emoji`, `webhook_display_name` | the username/icon overrides, gated on two settings this port refuses on |
/// | `mm_blocks_actions` | the strip is conditional on the session being an integration |
/// | `current_team_id` | `FillInPostProps` deletes it, but only inside the channel-mention branch that is refused |
/// | `ai_generated_by` | resolved to a username through a user lookup |
///
/// **Hardened mode does not protect this list.** `ExperimentalEnableHardenedMode` is off by
/// default, so `from_webhook` and the three overrides really are settable by any client on the
/// public create-post API — which is why they are refused here rather than assumed absent.
const REFUSED_CREATE_PROPS: [&str; 9] = [
    POST_PROPS_FROM_WEBHOOK,
    POST_PROPS_FROM_BOT,
    POST_PROPS_FROM_PLUGIN,
    POST_PROPS_OVERRIDE_USERNAME,
    POST_PROPS_OVERRIDE_ICON_URL,
    POST_PROPS_OVERRIDE_ICON_EMOJI,
    POST_PROPS_WEBHOOK_DISPLAY_NAME,
    POST_PROPS_MM_BLOCKS_ACTIONS,
    POST_PROPS_CURRENT_TEAM_ID,
];

impl App {
    // -- the deduplication cache ------------------------------------------------------------
    //
    // Go's `Server.seenPendingPostIdsCache`, an LRU with a 30-second expiry. Modelled as a map
    // because the *semantics* are load-bearing and the eviction policy is not: three branches of
    // `deduplicateCreatePost` read whether the key is absent, present-but-unknown, or present
    // with an id, and each answers differently.
    //
    // While the Go server is also running the two caches are independent, exactly as the status
    // cache is ([D-191]) — a post Go created is not deduplicated here and vice versa. See [D-400].

    /// `a.Srv().seenPendingPostIdsCache.Get(pendingPostId, &postID)`.
    ///
    /// `None` is Go's `cache.ErrKeyNotFound`; `Some(entry)` with an empty `post_id` is the claimed
    /// -but-unsaved state. Expired entries are treated as absent and dropped on the way past,
    /// which is the LRU's own behaviour.
    fn pending_post_id_get(&self, pending_post_id: &str) -> Option<PendingPostEntry> {
        let mut cache = self
            .pending_post_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = std::time::Instant::now();
        match cache.get(pending_post_id) {
            Some(entry) if entry.expires_at > now => Some(entry.clone()),
            Some(_) => {
                cache.remove(pending_post_id);
                None
            }
            None => None,
        }
    }

    /// `a.Srv().seenPendingPostIdsCache.SetWithExpiry(pendingPostId, postId, ttl)`.
    fn pending_post_id_set(&self, pending_post_id: &str, post_id: &str) {
        self.pending_post_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                pending_post_id.to_owned(),
                PendingPostEntry {
                    post_id: post_id.to_owned(),
                    expires_at: std::time::Instant::now() + PENDING_POST_IDS_CACHE_TTL,
                },
            );
    }

    /// `a.Srv().seenPendingPostIdsCache.Remove(pendingPostId)` — the `defer` in `CreatePost`
    /// that lets a client retry after a failure.
    fn pending_post_id_remove(&self, pending_post_id: &str) {
        self.pending_post_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(pending_post_id);
    }

    /// Port of `app.App.deduplicateCreatePost` (app/post.go:124), split at Go's cache `Get`.
    ///
    /// # Three outcomes, and the middle one is a 500
    ///
    /// An absent key means "go ahead". A key holding the empty string means another request
    /// claimed this pending id and has not finished saving — Go answers **500
    /// `api.post.deduplicate_create_post.pending`** rather than waiting. A key holding a post id
    /// means the first request finished, and the *same post* is returned so the call feels
    /// idempotent.
    ///
    /// # A 403 on the stored post is not an error
    ///
    /// `GetPostIfAuthorized` returning 403 is logged and **ignored**, falling through to a normal
    /// create. Any other failure is 500 `api.post.deduplicate_create_post.failed_to_get`. So a
    /// pending id that collides with another user's post creates a second post rather than
    /// leaking the first.
    async fn deduplicate_create_post(
        &self,
        post: &Post,
        session: &Session,
    ) -> Result<Option<Post>, PrepareError> {
        // "We rely on the client sending the pending post id across duplicate requests. If there
        // isn't one, we can't deduplicate, so allow creation normally."
        if post.pending_post_id.is_empty() {
            return Ok(None);
        }

        let Some(entry) = self.pending_post_id_get(&post.pending_post_id) else {
            return Ok(None);
        };

        if entry.post_id == UNKNOWN_POST_ID {
            return Err(PrepareError::App(AppError::boxed(
                "deduplicateCreatePost",
                "api.post.deduplicate_create_post.pending",
                None,
                String::new(),
                500,
            )));
        }

        match self
            .get_post_if_authorized(&entry.post_id, session, false)
            .await
        {
            Ok((found, _is_member)) => {
                tracing::debug!(
                    post_id = %found.id,
                    pending_post_id = %post.pending_post_id,
                    "Deduplicated create post",
                );
                Ok(Some(found))
            }
            Err(err) if err.status_code == 403 => {
                tracing::warn!(
                    pending_post_id = %post.pending_post_id,
                    post_id = %entry.post_id,
                    "Ignoring pending_post_id for which the user is unauthorized",
                );
                Ok(None)
            }
            Err(_) => Err(PrepareError::App(AppError::boxed(
                "deduplicateCreatePost",
                "api.post.deduplicate_create_post.failed_to_get",
                None,
                String::new(),
                500,
            ))),
        }
    }

    // -- the refusal gate -------------------------------------------------------------------

    /// Every shape of `POST /api/v4/posts` this server hands to Go, decided **before any write**.
    ///
    /// Ordered cheapest-first, but the order is not observable: every arm returns the same
    /// [`PrepareError::Unreproducible`], which the handler turns into a proxied request rather
    /// than a response. What *is* observable is that this runs before
    /// [`App::pending_post_id_set`] and before `Post().Save`, so a forwarded request has left no
    /// row, no channel counter and no cache entry behind.
    ///
    /// # The mention checks are three, not one
    ///
    /// `SendNotifications` writes `IncrementMentionCount` for every user a post mentions, and
    /// `getExplicitMentions` finds them three ways: an `@`-token, a `~channel` link, and a
    /// **keyword** from a member's own `mention_keys` or first name — which can be any word at
    /// all. The first two are string tests; the third is a query, because there is no way to read
    /// it off the message.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id))]
    async fn refuse_create_post_shapes(
        &self,
        post: &Post,
        channel: &Channel,
    ) -> Result<(), PrepareError> {
        // A reply's *refusals* are served — see [`App::resolve_root_post`], which runs at Go's
        // position further down — and only a reply that clears them is forwarded, from there. It
        // is deliberately not refused here: moving the check earlier would answer a
        // reply-to-a-reply with Go's body only by accident, and answer some other request with
        // the wrong error.
        //
        // Anything but the default type takes a branch of its own: `card` reads
        // `FeatureFlags.IntegratedBoards`, `burn_on_read` writes TemporaryPost, `custom_*` is a
        // plugin's, and the rest skip the `use_channel_mentions` gate.
        if !post.post_type.is_empty() {
            return Err(PrepareError::Unreproducible(
                "a non-default post type takes a branch of its own in CreatePost",
            ));
        }
        // `attachFilesToPost` re-parents FileInfo rows and can `Overwrite` the post.
        if post.file_ids.as_deref().is_some_and(|ids| !ids.is_empty()) {
            return Err(PrepareError::Unreproducible(
                "attachFilesToPost writes FileInfo.PostId, which has no port",
            ));
        }
        // `savePostsPriority` and `savePostsPersistentNotifications` write two more tables.
        if post.get_priority().is_some() {
            return Err(PrepareError::Unreproducible(
                "savePostsPriority writes PostsPriority, which has no port",
            ));
        }
        // Nothing else in `Metadata` survives the store — it is recomputed by
        // `PreparePostForClient` — but a client that sends one is a client taking a shape this
        // port has not measured.
        if post.metadata.is_some() {
            return Err(PrepareError::Unreproducible(
                "an inbound metadata document is a shape this port has not measured",
            ));
        }
        if post.post_type.starts_with(POST_CUSTOM_TYPE_PREFIX) {
            return Err(PrepareError::Unreproducible("plugin post type"));
        }

        // `Channel.IsShared` — `CreatePost`'s own DM/GM refusal reads it, and every shape of it
        // needs the shared-channel sync service.
        if channel.is_shared() {
            return Err(PrepareError::Unreproducible(
                "a shared channel needs the shared-channel sync service",
            ));
        }
        // A DM or group message adds `SendAutoResponseIfNecessary` (which writes a second post),
        // the self-DM and bot-DM burn-on-read arms, and a `channel_display_name` built from the
        // sorted member list.
        if channel.channel_type != CHANNEL_TYPE_OPEN && channel.channel_type != CHANNEL_TYPE_PRIVATE
        {
            return Err(PrepareError::Unreproducible(
                "a DM or group message adds the auto-responder, which writes a second post",
            ));
        }

        for prop in REFUSED_CREATE_PROPS {
            if post.get_prop(prop).is_some() {
                return Err(PrepareError::Unreproducible(
                    "a prop that names an integration or an override changes a branch here",
                ));
            }
        }
        if post.get_prop(POST_PROPS_AI_GENERATED_BY_USER_ID).is_some() {
            return Err(PrepareError::Unreproducible(
                "ai_generated_by is resolved to a username through a user lookup",
            ));
        }
        // The eight props `PreparePostForClient` cannot reproduce, refused here so the refusal
        // lands before the row rather than after it.
        crate::post::refuse_on_props(post)?;

        // `getEmbedsAndImages` → `getFirstLink` → `getLinkMetadata`, and a permalink additionally
        // writes the `previewed_post` prop into the saved row.
        if message_may_contain_a_link(&post.message) {
            return Err(PrepareError::Unreproducible(
                "message may contain a link or a markdown image",
            ));
        }
        // `FillInPostProps` resolves `~name` into the `channel_mentions` prop, and
        // `getExplicitMentions` counts the channel's members as mentioned.
        if post.message.contains('~') {
            return Err(PrepareError::Unreproducible(
                "a ~channel mention resolves channels and teams into a prop",
            ));
        }
        // `@here`, `@all`, `@channel`, `@username` and the group mentions all need the `@`, and
        // every one of them ends in `IncrementMentionCount`.
        if post.message.contains('@') {
            return Err(PrepareError::Unreproducible(
                "an @-mention reaches IncrementMentionCount and the notification fan-out",
            ));
        }

        // `handleWebhookEvents` fires an outgoing webhook whose *response* Go turns into a second
        // post. Its own two gates come first and both are cheap: `EnableOutgoingWebhooks`, then
        // `channel.Type != ChannelTypeOpen` — a private channel never triggers one however many
        // hooks the team has. Only then is the team's hook list worth reading, and Go reads the
        // whole list too (`GetOutgoingByTeam(team.Id, -1, -1)`).
        if self.config().enable_outgoing_webhooks && channel.channel_type == CHANNEL_TYPE_OPEN {
            let hooks = self
                .store()
                .webhook()
                .get_outgoing_by_team_unpaged(&channel.team_id)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "outgoing webhook lookup failed");
                    PrepareError::Unreproducible("the outgoing webhook lookup failed")
                })?;
            if !hooks.is_empty() {
                return Err(PrepareError::Unreproducible(
                    "an outgoing webhook turns its response into a second post",
                ));
            }
        }

        // The keyword half of `getExplicitMentions`: a member whose `mention_keys` is non-empty,
        // or whose `first_name` notification is on, can be mentioned by a message with no `@` in
        // it at all.
        if self
            .store()
            .post()
            .channel_has_keyword_mention_recipients(&channel.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "keyword mention recipient lookup failed");
                PrepareError::Unreproducible("the keyword mention recipient lookup failed")
            })?
        {
            return Err(PrepareError::Unreproducible(
                "a channel member can be mentioned by a keyword rather than by an @",
            ));
        }

        Ok(())
    }

    // -- the write --------------------------------------------------------------------------

    /// Port of `app.App.CreatePostAsUserWithFlags` (app/post.go:42).
    ///
    /// # Four gates before `CreatePost`, and the first one flattens every store error into a 400
    ///
    /// `Store().Channel().Get` is called directly rather than through `App.GetChannel`, and its
    /// error — missing row or database fault alike — becomes
    /// `api.context.invalid_param.app_error` with `Name: post.channel_id` at **400**. A port that
    /// reached for `App::get_channel` would answer 404 `app.channel.get.app_error` for a channel
    /// that is not there, which is a different id on the wire.
    ///
    /// # `MarkChannelsAsViewed` is skipped for three different reasons
    ///
    /// `from_webhook`, `from_bot` and "a reply with CRT on". The first two are refused by
    /// [`App::refuse_create_post_shapes`] as *inbound* props, but `from_bot` is also **added by
    /// `CreatePost` itself** when the author is a bot — so the surviving condition here is the
    /// author's bot flag, not the request's.
    #[tracing::instrument(skip_all, fields(channel_id = %post.channel_id, forwarded))]
    pub async fn create_post_as_user(
        &self,
        post: Post,
        session: &Session,
        flags: CreatePostFlags,
    ) -> Result<Post, PrepareError> {
        let channel = self
            .store()
            .channel()
            .get(&post.channel_id)
            .await
            .map_err(|err| {
                tracing::debug!(error = %err, "create post could not read the channel");
                invalid_param("CreatePostAsUser", "post.channel_id")
            })?;

        // Before the archive check: a `system_*` type in a deleted channel is a `post.type` error,
        // not a `can_not_post_to_deleted` one.
        if post.post_type.starts_with(POST_SYSTEM_MESSAGE_PREFIX) {
            return Err(invalid_param("CreatePostAsUser", "post.type"));
        }

        if channel.delete_at != 0 {
            return Err(PrepareError::App(AppError::boxed(
                "createPost",
                "api.post.create_post.can_not_post_to_deleted.error",
                None,
                String::new(),
                400,
            )));
        }

        match self.check_if_channel_is_restricted_dm(&channel).await? {
            RestrictedDm::Yes => {
                return Err(PrepareError::App(AppError::boxed(
                    "createPost",
                    "api.post.create_post.can_not_post_in_restricted_dm.error",
                    None,
                    String::new(),
                    400,
                )));
            }
            RestrictedDm::Undecidable => {
                return Err(PrepareError::Unreproducible(
                    "a bot member's DM-restriction exemption is a plugin decision",
                ));
            }
            RestrictedDm::No => {}
        }

        let (saved, author_is_bot) = self.create_post(post, &channel, session, flags).await?;

        // `_, fromWebhook := post.GetProps()[from_webhook]` and the `from_bot` twin. Both props
        // are refused inbound, so only the bot flag `CreatePost` derives survives; `isCRTReply`
        // needs a `root_id`, also refused.
        if !author_is_bot {
            let is_crt_enabled = self.is_crt_enabled_for_user(&saved.user_id).await;
            if let Err(err) = self
                .mark_channels_as_viewed(
                    std::slice::from_ref(&saved.channel_id),
                    &saved.user_id,
                    true,
                    is_crt_enabled,
                )
                .await
            {
                // Go logs and continues — the post is already written.
                tracing::warn!(
                    error = %err,
                    channel_id = %saved.channel_id,
                    user_id = %saved.user_id,
                    "Encountered error updating last viewed",
                );
            }
        }

        Ok(saved)
    }

    /// Port of `app.App.CreatePost` (app/post.go:173) for the shape
    /// [`App::refuse_create_post_shapes`] admits. Returns the prepared post and whether its author
    /// is a bot, which is the only thing the caller still needs off the pre-save post.
    ///
    /// # Stage order is Go's, and the pending-post id sits across the middle of it
    ///
    /// The cache lookup is Go's — early, before the author lookup — so a duplicate request answers
    /// with the first post rather than with whatever validation the second one would trip on. The
    /// *claim* is Go's `defer` unrolled: set on entry, removed on any failure, overwritten with
    /// the saved id on success. Unrolling it is what makes a **forward** safe: the forward is a
    /// failure as far as this function is concerned, so the entry is removed and the retry Go
    /// serves is deduplicated by Go's own cache instead.
    async fn create_post(
        &self,
        mut post: Post,
        channel: &Channel,
        session: &Session,
        flags: CreatePostFlags,
    ) -> Result<(Post, bool), PrepareError> {
        if let Some(found) = self.deduplicate_create_post(&post, session).await? {
            return Ok((found, false));
        }

        // Everything this port cannot answer, decided before the claim and before the row.
        self.refuse_create_post_shapes(&post, channel).await?;

        let pending_post_id = post.pending_post_id.clone();
        if !pending_post_id.is_empty() {
            self.pending_post_id_set(&pending_post_id, UNKNOWN_POST_ID);
        }

        let outcome = self
            .create_post_claimed(&mut post, channel, session, flags)
            .await;

        if !pending_post_id.is_empty() {
            match outcome.as_ref() {
                Ok((saved, _)) => self.pending_post_id_set(&pending_post_id, &saved.id),
                Err(_) => self.pending_post_id_remove(&pending_post_id),
            }
        }

        outcome
    }

    /// The body of `CreatePost` between the deduplication claim and its `defer`.
    async fn create_post_claimed(
        &self,
        post: &mut Post,
        channel: &Channel,
        session: &Session,
        flags: CreatePostFlags,
    ) -> Result<(Post, bool), PrepareError> {
        // `flags.SilentNotification` with persistent notifications on is a 400 before anything
        // else looks at either. The persistent-notification post itself is refused by the store,
        // so only the refusal arm is reachable.
        if flags.silent_notification && post.get_persistent_notification() == Some(true) {
            return Err(PrepareError::App(AppError::boxed(
                "CreatePost",
                "api.post.create_post.silent_persistent_notification.app_error",
                None,
                String::new(),
                400,
            )));
        }

        // `SanitizeProps` strips `add_channel_member` always, and `force_notification` and
        // `silent_notification` unless the post is federated — which a REST create never is,
        // because `SanitizeInput` has already set `remote_id` to the empty string.
        post.sanitize_props();

        let user = self.get_user(&post.user_id).await.map_err(|mut err| {
            // Go's own ids: `MissingAccountError` at 404, `app.user.get.app_error` at 500.
            err.where_ = "CreatePost".to_owned();
            err
        })?;

        if user.is_bot {
            post.add_prop(
                POST_PROPS_FROM_BOT,
                serde_json::Value::String("true".to_owned()),
            );
        }

        if flags.silent_notification {
            // `isIntegrationPostAuthor` — deliberately narrower than `Session.IsIntegration()`:
            // a personal access token is *not* an integration here.
            if !(user.is_bot || session.is_oauth) {
                tracing::warn!(
                    user_id = %user.id,
                    channel_id = %channel.id,
                    "Rejected silent notification post from non-integration author",
                );
                return Err(PrepareError::App(AppError::boxed(
                    "CreatePost",
                    "api.post.create_post.silent_notification.app_error",
                    None,
                    String::new(),
                    403,
                )));
            }
            post.add_prop(
                POST_PROPS_SILENT_NOTIFICATION,
                serde_json::Value::Bool(true),
            );
            // The prop is on the saved row, but suppressing the notification it names is the
            // fan-out's job and the fan-out is not here.
            return Err(PrepareError::Unreproducible(
                "a silent notification post suppresses a fan-out this port does not run",
            ));
        }

        if session.is_oauth {
            post.add_prop(
                POST_PROPS_FROM_OAUTH_APP,
                serde_json::Value::String("true".to_owned()),
            );
        }

        // The `post.Type == ""` guard is always taken — a non-default type is refused — so what
        // decides is the permission. Without it Go rewrites `@channel`/`@all`/`@here` and sends
        // the author an ephemeral notice; with an `@` refused upstream there is nothing to
        // rewrite, but the permission still has to be read, because a mutation that drops the
        // call would otherwise be invisible.
        if post.post_type.is_empty() {
            let (has_permission, _) = self
                .has_permission_to_channel(&user.id, &channel.id, &PERMISSION_USE_CHANNEL_MENTIONS)
                .await;
            if !has_permission && post.disable_mention_highlights().is_some() {
                return Err(PrepareError::Unreproducible(
                    "a channel-wide mention without use_channel_mentions sends an ephemeral notice",
                ));
            }
        }

        // "Verify the parent/child relationships are correct." Go started this read in a
        // goroutine at the top of `CreatePost` and consumes it **here** — after the author
        // lookup, after the props and after the mention gate — so a reply whose author is
        // missing is a 404 and not a root-id 400.
        self.resolve_root_post(post, channel).await?;

        let (hashtags, _plain_text) = parse_hashtags(&post.message);
        post.hashtags = hashtags;

        // `FillInPostProps` reduces to its `else if post.GetProps() != nil` arm: every other
        // branch needs a `~` mention, an `@` on a licensed server, an `ai_generated_by` prop or a
        // burn-on-read type, and all four are refused.
        self.fill_in_post_props(post).await?;

        // `runGuardedMessageWillBePosted` — no plugin environment, [D-183].

        // "Pre-fill the CreateAt field for link previews to get the correct timestamp." A
        // client-supplied `CreateAt` survives; the handler has already zeroed it unless the
        // caller holds `manage_system`.
        if post.create_at == 0 {
            post.create_at = get_millis();
        }

        // `getEmbedsAndImages` leaves `Embeds` empty and `Images` empty on a message with no
        // link, and `omitempty` drops both — so there is no `previewed_post` prop to add either.

        let saved = self.store().post().save(post).await.map_err(|err| {
            if let mm_store::StoreError::Invalid { app_error, .. } = err {
                // `errors.As(nErr, &appErr)` — `IsValid`'s own error reaches the client verbatim.
                return PrepareError::App(app_error);
            }
            if let mm_store::StoreError::Argument { detail, .. } = err {
                // The store's own refusals. Every shape that reaches one is refused above, so
                // this is a belt-and-braces forward rather than a reachable path; reaching it
                // after the insert is impossible because the refusals precede it.
                tracing::debug!(detail, "the post store refused the save");
                return PrepareError::Unreproducible("the post store refused this post shape");
            }
            tracing::error!(error = %err, "post save failed");
            PrepareError::App(AppError::boxed(
                "CreatePost",
                "app.post.save.app_error",
                None,
                String::new(),
                500,
            ))
        })?;

        // `attachFilesToPost` — no file ids, refused above.
        // `MessageHasBeenPosted` — no plugin environment, [D-183].

        // `PreparePostForClient`, *not* the embeds-and-images variant: Go relies on
        // `getEmbedsAndImages` having already run on the pre-save post.
        let prepared = self
            .prepare_post_for_client(
                &saved,
                PreparePostForClientOpts {
                    is_edit_post: true,
                    ..PreparePostForClientOpts::default()
                },
            )
            .await?;

        // `applyPostWillBeConsumedHook`, `ResolvePersistentNotification` and the `ThreadAutoFollow`
        // membership are all plugin- or reply-shaped. What is left of `handlePostEvents` is the
        // `posted` event.
        self.publish_user_posted_event(&prepared, channel, &user, flags.set_online)
            .await;

        let (sanitized, _is_member_for_previews) = self
            .sanitize_post_metadata_for_user(prepared, &session.user_id)
            .await?;

        Ok((sanitized, user.is_bot))
    }

    /// The parent/child verification in `CreatePost` (app/post.go:325), and the forward that
    /// follows it.
    ///
    /// # Four outcomes and only one of them is a 500 in Go's own numbering
    ///
    /// A root that cannot be read is `api.post.create_post.root_id.app_error` at 400. A thread
    /// whose posts are not all in this channel is `api.post.create_post.channel_root_id.app_error`
    /// — constructed at **500** and then *rewritten to 400* by `CreatePostAsUserWithFlags`, which
    /// tests `err.Id` against exactly those two ids and lowers the status. Reproducing the 500
    /// would be a status divergence on a body that is otherwise identical.
    ///
    /// # `Post().Get(rootId)` resolves the whole thread, not the one row
    ///
    /// So replying to a **reply** finds the reply in the returned map with a non-empty `RootId`,
    /// and that is the reply-to-a-reply refusal: `api.post.create_post.root_id.app_error` at 400
    /// again, the same id as an unreadable root. A client cannot tell the two apart, and neither
    /// can a log reader with only the id.
    ///
    /// # A reply that passes every check is then handed to Go
    ///
    /// `SqlPostStore.Save` calls `updateThreadsFromPosts`, which writes a `Threads` row and a
    /// `ThreadMemberships` row; `ResolvePersistentNotification` and the CRT follower fan-out hang
    /// off the same field. None is ported — so the refusals are served, the success is forwarded,
    /// and the forward is before `Post().Save` and before anything else writes.
    async fn resolve_root_post(&self, post: &Post, channel: &Channel) -> Result<(), PrepareError> {
        if post.root_id.is_empty() {
            return Ok(());
        }

        let root_id_error = || {
            PrepareError::App(AppError::boxed(
                "createPost",
                "api.post.create_post.root_id.app_error",
                None,
                String::new(),
                400,
            ))
        };

        let parent = self
            .store()
            .post()
            .get_thread(
                &post.root_id,
                GetPostThreadOptions {
                    user_id: "",
                    skip_fetch_threads: false,
                    collapsed_threads: false,
                    updates_only: false,
                    per_page: 0,
                    // Go passes a zero `GetPostsOptions`, so there is no `ORDER BY` at all.
                    direction: ThreadDirection::Unset,
                    from_post: "",
                    from_create_at: 0,
                    from_update_at: 0,
                },
            )
            .await
            .map_err(|err| {
                tracing::debug!(error = %err, root_id = %post.root_id, "the root post could not be read");
                root_id_error()
            })?;

        let posts = parent.posts.unwrap_or_default();

        // `len(parentPostList.Posts) == 0 || !parentPostList.IsChannelId(post.ChannelId)` — the
        // second half is an **all**, not an any: every post in the thread has to be in this
        // channel, which is how a `root_id` borrowed from another channel is caught.
        if posts.is_empty() || posts.values().any(|p| p.channel_id != channel.id) {
            return Err(PrepareError::App(AppError::boxed(
                "createPost",
                "api.post.create_post.channel_root_id.app_error",
                None,
                String::new(),
                // Constructed 500 by `CreatePost` and lowered to 400 by its caller, which is the
                // only status a client ever sees for this id.
                400,
            )));
        }

        let Some(root) = posts.get(&post.root_id) else {
            // Go indexes the map and dereferences the result; a miss would be a nil dereference.
            // The thread always contains the post it was fetched by, so this is unreachable.
            return Err(root_id_error());
        };
        if !root.root_id.is_empty() {
            return Err(root_id_error());
        }
        if root.post_type == POST_TYPE_BURN_ON_READ {
            return Err(PrepareError::App(AppError::boxed(
                "createPost",
                "api.post.create_post.burn_on_read.app_error",
                None,
                String::new(),
                400,
            )));
        }

        Err(PrepareError::Unreproducible(
            "a reply needs updateThreadsFromPosts and the CRT follower fan-out",
        ))
    }

    /// The `posted` event `SendNotifications` builds (notification.go:699) for a **user** post.
    ///
    /// # Both names are formatted with `ShowUsername`, and that is a constant, not a setting
    ///
    /// `GetChannelName(model.ShowUsername, "")` and `GetSenderName(model.ShowUsername, …)` — the
    /// literal constant is passed at both call sites, so neither the `TeammateNameDisplay` setting
    /// nor the caller's `name_format` preference is read. A port that helpfully threaded
    /// `GetNotificationNameFormat` through here would send a different `sender_name` on any server
    /// whose users had set a display preference.
    ///
    /// # `sender_name` carries an `@` and `channel_display_name` does not
    ///
    /// `GetDisplayNameWithPrefix(…, "@")` against `GetDisplayName(…)`. For an open or private
    /// channel `GetChannelName` returns `Channel.DisplayName` untouched.
    ///
    /// # Three keys are absent and one is
    ///
    /// `otherFile` and `image` need file ids; `add_mentions`, `add_followers` and `posted_ack` are
    /// broadcast hooks, which the hub strips ([D-183]).
    async fn publish_user_posted_event(
        &self,
        post: &Post,
        channel: &Channel,
        sender: &User,
        set_online: bool,
    ) {
        // `SendNotifications` opens with `if channel.DeleteAt > 0 { return }`. Unreachable from
        // this route — `CreatePostAsUser` refuses an archived channel — but the guard is the
        // function's, not the caller's.
        if channel.delete_at > 0 {
            return;
        }

        let mut message =
            WebSocketEvent::new(WEBSOCKET_EVENT_POSTED, "", &post.channel_id, "", None, "");
        message.add(
            "channel_type",
            serde_json::Value::String(channel.channel_type.clone()),
        );
        message.add(
            "channel_display_name",
            serde_json::Value::String(channel.display_name.clone()),
        );
        message.add(
            "channel_name",
            serde_json::Value::String(channel.name.clone()),
        );
        message.add(
            "sender_name",
            serde_json::Value::String(sender.get_display_name_with_prefix(SHOW_USERNAME, "@")),
        );
        // `team.Id`, and Go substitutes an empty `model.Team{}` for a DM — which cannot reach
        // here, so the channel's team id is the team's id.
        message.add(
            "team_id",
            serde_json::Value::String(channel.team_id.clone()),
        );
        message.add("set_online", serde_json::Value::Bool(set_online));

        match post.to_json() {
            Ok(json) => message.add("post", serde_json::Value::String(json)),
            Err(err) => {
                tracing::error!(error = %err, post_id = %post.id, "Error in marshalling post to JSON");
                return;
            }
        }

        self.publish(message).await;
    }

    /// Port of `app.App.SendEphemeralPost` (app/post.go:759).
    ///
    /// # It writes nothing
    ///
    /// No `Post().Save`, no channel counters, no thread rows — the post is built in memory,
    /// prepared for the client, pushed down one user's websocket and returned. That is what makes
    /// `POST /api/v4/posts/ephemeral` a much smaller port than its sibling: the only failure mode
    /// is answering with a *shape* Go would have answered differently, never a row Go would not
    /// have written.
    ///
    /// # The event is addressed to `user_id`, not to the channel
    ///
    /// `NewWebSocketEvent(ephemeral_message, "", post.ChannelId, userID, nil, "")` carries both a
    /// channel and a user, and the hub's targeting takes the user — so the caller's own client
    /// sees it only when the caller is also the recipient.
    ///
    /// # `Type` is overwritten, not validated
    ///
    /// The first line is `post.Type = model.PostTypeEphemeral`, so whatever the client asked for
    /// is discarded. `GenerateActionIds` and `AddPostActionCookies` walk `props.attachments`,
    /// which is refused by `PreparePostForClient`, so both are no-ops on every shape served here.
    #[tracing::instrument(skip_all, fields(channel_id = %post.channel_id))]
    pub async fn send_ephemeral_post(
        &self,
        user_id: &str,
        mut post: Post,
    ) -> Result<Post, PrepareError> {
        post.post_type = POST_TYPE_EPHEMERAL.to_owned();

        if post.id.is_empty() {
            post.id = new_id();
        }
        if post.create_at == 0 {
            post.create_at = get_millis();
        }
        if post.get_props().is_none() {
            post.set_props(Some(mm_model::utils::StringInterface::new()));
        }

        // `GenerateActionIds`, then the prepare, then `AddPostActionCookies` — all three read
        // `props.attachments`, which `prepare_post_for_client` refuses.
        let post = self
            .prepare_post_for_client_with_embeds_and_images(
                &post,
                PreparePostForClientOpts {
                    is_new_post: true,
                    include_priority: true,
                    ..PreparePostForClientOpts::default()
                },
            )
            .await?;

        let (sanitized, _is_member_for_previews) =
            self.sanitize_post_metadata_for_user(post, user_id).await?;

        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_EPHEMERAL_MESSAGE,
            "",
            &sanitized.channel_id,
            user_id,
            None,
            "",
        );
        match sanitized.to_json() {
            Ok(json) => message.add("post", serde_json::Value::String(json)),
            Err(err) => {
                // Go warn-logs and adds the *empty* string, then publishes anyway.
                tracing::warn!(error = %err, "Failed to encode post to JSON");
                message.add("post", serde_json::Value::String(String::new()));
            }
        }
        self.publish(message).await;

        Ok(sanitized)
    }

    /// Port of `PostBurnOnReadCheckWithApp` (app/post_permission_utils.go:130) for the arms a
    /// create can reach with the channel already in hand.
    ///
    /// The api4 wrapper calls it with a **nil** channel, which is why the lookup and its 500 are
    /// part of the check rather than the caller's job.
    pub async fn post_burn_on_read_check(
        &self,
        where_: &'static str,
        user_id: &str,
        channel_id: &str,
        post_type: &str,
    ) -> Result<(), PrepareError> {
        if post_type != POST_TYPE_BURN_ON_READ {
            return Ok(());
        }

        let channel = self
            .store()
            .channel()
            .get(channel_id)
            .await
            .map_err(|err| {
                tracing::debug!(error = %err, "burn-on-read check could not read the channel");
                PrepareError::App(AppError::boxed(
                    where_,
                    "api.post.fill_in_post_props.burn_on_read.channel.app_error",
                    None,
                    String::new(),
                    500,
                ))
            })?;

        if channel.is_shared() {
            return Err(PrepareError::App(AppError::boxed(
                where_,
                "api.post.fill_in_post_props.burn_on_read.shared_channel.app_error",
                None,
                String::new(),
                400,
            )));
        }

        if channel.channel_type == mm_model::channel::CHANNEL_TYPE_DIRECT {
            // `GetDMNameFromIds(userId, userId)` — a self-DM's name is the id joined to itself.
            if channel.name == mm_model::channel::get_dm_name_from_ids(user_id, user_id) {
                return Err(PrepareError::App(AppError::boxed(
                    where_,
                    "api.post.fill_in_post_props.burn_on_read.self_dm.app_error",
                    None,
                    String::new(),
                    400,
                )));
            }

            let other_user_id = get_other_user_id_for_dm(&channel.name, user_id);
            if !other_user_id.is_empty() && other_user_id != user_id {
                let other = self.get_user(&other_user_id).await.map_err(|err| {
                    tracing::debug!(error = %err, "burn-on-read check could not read the other user");
                    PrepareError::App(AppError::boxed(
                        where_,
                        "api.post.fill_in_post_props.burn_on_read.user.app_error",
                        None,
                        String::new(),
                        500,
                    ))
                })?;
                if other.is_bot {
                    return Err(PrepareError::App(AppError::boxed(
                        where_,
                        "api.post.fill_in_post_props.burn_on_read.bot_dm.app_error",
                        None,
                        String::new(),
                        400,
                    )));
                }
            }
        }

        Ok(())
    }
}

/// Port of `(*Channel).GetOtherUserIdForDM` (channel.go) — the DM name is `id1__id2` sorted, so
/// the other party is whichever half is not the caller. Returns `""` for a name that is not a DM
/// name, which is Go's answer too.
fn get_other_user_id_for_dm(channel_name: &str, user_id: &str) -> String {
    let Some((first, second)) = channel_name.split_once("__") else {
        return String::new();
    };
    if first == user_id {
        second.to_owned()
    } else if second == user_id {
        first.to_owned()
    } else {
        String::new()
    }
}

/// `model.NewAppError(where, "api.context.invalid_param.app_error", map[string]any{"Name": name},
/// "", http.StatusBadRequest)`.
fn invalid_param(where_: &'static str, name: &'static str) -> PrepareError {
    let mut params: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    params.insert("Name".to_owned(), serde_json::json!(name));
    PrepareError::App(AppError::boxed(
        where_,
        "api.context.invalid_param.app_error",
        Some(params),
        String::new(),
        400,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post_with_message(message: &str) -> Post {
        Post {
            message: message.to_owned(),
            ..Post::default()
        }
    }

    #[test]
    fn a_self_dm_name_has_the_same_id_on_both_sides() {
        // `GetDMNameFromIds(a, a)` is `a__a`, so the "other" party is the caller.
        assert_eq!(get_other_user_id_for_dm("aaa__aaa", "aaa"), "aaa");
    }

    #[test]
    fn the_other_party_is_whichever_half_is_not_the_caller() {
        assert_eq!(get_other_user_id_for_dm("aaa__bbb", "aaa"), "bbb");
        assert_eq!(get_other_user_id_for_dm("aaa__bbb", "bbb"), "aaa");
    }

    #[test]
    fn a_name_that_is_not_a_dm_name_has_no_other_party() {
        // Go returns "" rather than guessing, and so must this: a channel whose name happens to
        // contain no `__` is not a DM at all.
        assert_eq!(get_other_user_id_for_dm("town-square", "aaa"), "");
        assert_eq!(get_other_user_id_for_dm("aaa__bbb", "ccc"), "");
    }

    #[test]
    fn the_mention_needles_are_the_three_getexplicitmentions_reads() {
        // `@` covers @username/@all/@here/@channel and the group mentions; `~` covers the channel
        // link. A message with neither still has to clear the keyword query, which is why this
        // test asserts the *string* half only.
        assert!(post_with_message("hi @sam").message.contains('@'));
        assert!(post_with_message("see ~town-square").message.contains('~'));
        assert!(!post_with_message("plain words").message.contains('@'));
        assert!(!post_with_message("plain words").message.contains('~'));
    }

    #[test]
    fn an_email_address_is_an_at_mention_for_this_gate_even_though_go_finds_no_link_in_it() {
        // Deliberate over-approximation: Mattermost's autolinker has no email rule, so
        // `message_may_contain_a_link` says no — but `@` says yes and the request is forwarded.
        // Widening is free here; narrowing would write a row Go would have mentioned somebody for.
        let post = post_with_message("mail me at sam@example.com");
        assert!(!message_may_contain_a_link(&post.message));
        assert!(post.message.contains('@'));
    }

    #[test]
    fn every_refused_create_prop_is_a_distinct_key() {
        let mut seen = std::collections::HashSet::new();
        for prop in REFUSED_CREATE_PROPS {
            assert!(seen.insert(prop), "{prop} is listed twice");
        }
        // The four that a client can really set with hardened mode off, named so that removing
        // one from the list fails here rather than silently serving a forged webhook post.
        for required in [
            POST_PROPS_FROM_WEBHOOK,
            POST_PROPS_OVERRIDE_USERNAME,
            POST_PROPS_OVERRIDE_ICON_URL,
            POST_PROPS_OVERRIDE_ICON_EMOJI,
        ] {
            assert!(seen.contains(required), "{required} must be refused");
        }
    }

    #[test]
    fn the_cache_ttl_is_go_s_thirty_seconds() {
        assert_eq!(PENDING_POST_IDS_CACHE_TTL.as_secs(), 30);
    }
}
