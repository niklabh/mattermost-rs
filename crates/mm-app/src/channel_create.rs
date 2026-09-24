//! Port of the create side of `server/channels/app/channel.go`: `CreateChannelWithUser` (:158),
//! `CreateChannel` (:238), `GetOrCreateDirectChannel` (:351), `createDirectChannel` (:444),
//! `createDirectChannelWithUser` (:477), `CreateGroupChannel` (:547) and `createGroupChannel`
//! (:572).
//!
//! Kept apart from [`crate::channel_write`] because they share nothing: a create validates a
//! channel that does not exist yet, writes *two* tables in one transaction, and publishes an
//! event whose addressing is different for each of the three routes.
//!
//! # The three routes are not one route with three types
//!
//! | | `POST /channels` | `/channels/direct` | `/channels/group` |
//! |---|---|---|---|
//! | body | a `Channel` | two user ids | three to eight user ids |
//! | name | the client's | `GetDMNameFromIds`, sorted | the SHA-1 of the sorted ids |
//! | team | required | forced empty | never set |
//! | creator's membership | `scheme_admin: true` | `scheme_admin` **false** | `scheme_admin` false |
//! | already exists | **400** `store.sql_channel.save_channel.exists.app_error` | 201, the existing channel | 201, the existing channel |
//! | event | `channel_created`, to the user | `direct_added`, to the channel | `group_added`, once per member |
//!
//! The "already exists" row is the one that surprises: `createChannel` is not idempotent and the
//! two message routes are, so the same `ChannelSave::Existing` from the store becomes a 400 on
//! one path and a success on the other two.
//!
//! # The creator's join post
//!
//! `CreateChannelWithUser` ends in `postJoinChannelMessage` (app/channel.go:2776), after a second
//! `GetUser` of the creator, and a failure of either fails the route with the channel already
//! written. Nothing in the 201 depends on the post — Go marshals the channel the store returned,
//! read before the post existed — but the next read of the channel sees its `last_post_at` and
//! `total_msg_count`, and a plugin sees both message hooks.
//!
//! # What is deliberately absent
//!
//! - `SetChannelManagedCategory` — `MinimumEnterpriseLicense` **and** a feature flag; on an
//!   unlicensed installation Go takes the `else` branch, logs a warning and **blanks
//!   `managed_category_name` on the answer**, which is reproduced because it is wire surface.
//! - `ShareChannel` for a DM or GM with a remote participant, when the shared-channel sync
//!   service would be running — that request is forwarded before anything is written (the
//!   service keeps its state in the Go process). With `FeatureFlags.EnableSharedChannelsDMs` off,
//!   Go's default, a remote participant is refused before that point.

use mm_model::channel::{
    CHANNEL_GROUP_MAX_USERS, CHANNEL_GROUP_MIN_USERS, CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP,
    Channel, get_dm_name_from_ids, get_group_display_name_from_users, get_group_name_from_user_ids,
};
use mm_model::channel_member::{ChannelMember, get_default_channel_notify_props};
use mm_model::sidebar_category::{
    SIDEBAR_CATEGORY_CUSTOM, SIDEBAR_CATEGORY_SORT_DEFAULT, SidebarCategory,
    SidebarCategoryWithChannels,
};
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_CHANNEL_CREATED, WEBSOCKET_EVENT_DIRECT_ADDED, WEBSOCKET_EVENT_GROUP_ADDED,
    WebSocketEvent,
};
use mm_store::channel_member_history_store::ChannelMemberHistoryStore;
use mm_store::channel_store::ChannelStore;
use mm_store::user_store::UserStore;
use mm_store::{ChannelSave, StoreError};

use crate::App;

/// `!FeatureFlags.EnableSharedChannelsDMs && <a participant is remote>` — the refusal
/// `createDirectChannelWithUser` (app/channel.go:478) and `createGroupChannel` (:603) share.
///
/// The flag is **false** by default (feature_flags.go:163) and settable only by the environment
/// at start ([`crate::config::Config::feature_flags`]). With it on, a remote participant is
/// admitted and the channel is created `shared`.
fn refuses_remote_participants(config: &crate::config::Config, any_remote: bool) -> bool {
    any_remote && !config.feature_flags.enable_shared_channels_dms
}

/// `FeatureFlags.EnableDocs` (feature_flags.go:104), **false** by default (:196): off,
/// `CreateChannel` refuses a space (type `S`) with a **403**. No REST route of this group reaches
/// it with the flag either way — `POST /channels` refuses type `S` twice before the app layer.
fn refuses_space(config: &crate::config::Config, channel: &Channel) -> bool {
    channel.is_space() && !config.feature_flags.enable_docs
}

/// The id `getSharedChannelsService` answers with when the sync service is nil
/// (app/shared_channel.go:35), which `ShareChannel` returns and a DM/GM create logs.
const SHARED_CHANNELS_SERVICE_DISABLED: &str = "api.command_share.service_disabled";

/// What a create did, or why it declined to do it.
///
/// Same shape and the same rule as [`crate::channel_write::ChannelWrite`]: **every `Forward` is
/// returned before anything is written**.
#[derive(Debug)]
pub enum ChannelCreate {
    /// The channel was created (or, for a DM or GM that already existed, found). The value is
    /// what the handler marshals.
    Created(Box<Channel>),
    /// Nothing was written. Forward the request whole.
    Forward(&'static str),
}

/// `store.ChannelExistsError` (store/constants.go:7).
const CHANNEL_EXISTS_ERROR: &str = "store.sql_channel.save_channel.exists.app_error";

/// `app.MissingAccountError` (app/constants.go:7).
const MISSING_ACCOUNT_ERROR: &str = "app.user.missing_account.const";

/// The `Save`/`SaveDirectChannel` error table, which four Go call sites spell out identically
/// apart from the handler name and two ids.
///
/// The `InvalidInput` field name is the discriminator, exactly as in Go's nested `switch`, and
/// the ids differ between the plain and direct paths — `save.direct_channel` versus
/// `save_direct_channel.not_direct` — so the caller supplies them.
fn save_channel_error(
    handler: &'static str,
    wrong_type_id: &'static str,
    err: StoreError,
) -> Box<AppError> {
    match err {
        StoreError::InvalidInput { field, value, .. } => match field {
            "DeleteAt" => AppError::boxed(
                handler,
                "store.sql_channel.save.archived_channel.app_error",
                None,
                String::new(),
                400,
            ),
            "Type" => AppError::boxed(handler, wrong_type_id, None, String::new(), 400),
            // `"id="+invErr.Value.(string)` reaches `detailed_error`, which is wiped unless
            // developer mode is on, so it is carried rather than dropped but never asserted.
            _ => AppError::boxed(
                handler,
                "store.sql_channel.save_channel.existing.app_error",
                None,
                format!("id={value}"),
                400,
            ),
        },
        StoreError::LimitExceeded { .. } => AppError::boxed(
            handler,
            "store.sql_channel.save_channel.limit.app_error",
            None,
            String::new(),
            400,
        ),
        // `errors.As(nErr, &appErr)` — the model's own `IsValid` error, passed through with its
        // `model.channel.is_valid.*` id and 400 intact.
        StoreError::Invalid { app_error, .. } => app_error,
        other => {
            tracing::error!(error = %other, "channel save failed");
            AppError::boxed(
                handler,
                "app.channel.create_channel.internal_error",
                None,
                String::new(),
                500,
            )
        }
    }
}

/// `SaveMember`'s error table on a create path.
///
/// The conflict id is `app.channel.save_member.exists.app_error` at **400**; everything else is
/// `app.channel.create_direct_channel.internal_error` at 500 — which is the id Go uses on the
/// *plain* create path too, not only the direct one.
fn save_member_error(handler: &'static str, err: StoreError) -> Box<AppError> {
    if err.conflict_resource() == Some("ChannelMembers") {
        return AppError::boxed(
            handler,
            "app.channel.save_member.exists.app_error",
            None,
            String::new(),
            400,
        );
    }
    if let StoreError::Invalid { app_error, .. } = err {
        return app_error;
    }
    tracing::error!(error = %err, "channel member save failed");
    AppError::boxed(
        handler,
        "app.channel.create_direct_channel.internal_error",
        None,
        String::new(),
        500,
    )
}

impl App {
    /// Port of `app.App.CreateChannelWithUser` (app/channel.go:158) — the whole of
    /// `POST /api/v4/channels` below the handler's own validation.
    ///
    /// # The limit is checked twice, with two different counts
    ///
    /// This function compares `GetNumberOfChannelsOnTeam() + 1` against
    /// `MaxChannelsPerTeam` and answers `api.channel.create_channel.max_channel_limit.app_error`;
    /// the store then compares a *different* count against the same setting and answers
    /// `store.sql_channel.save_channel.limit.app_error`. The first counts archived channels and
    /// group channels, the second does not, so which of the two 400s a client sees depends on the
    /// team's archive. Both are ported; see [`mm_store::channel_store`].
    ///
    /// # `channel` is mutated and the mutated value is what the store wrote
    ///
    /// `creator_id` is assigned here and `PreSave` fills in the id and timestamps inside the
    /// store, so the caller's channel is the created one afterwards — which is what the
    /// `channel_created` event and the sidebar step both read.
    #[tracing::instrument(skip_all, fields(team_id = %channel.team_id, channel_type = %channel.channel_type, user_id = %user_id))]
    pub async fn create_channel_with_user(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        channel: &mut Channel,
        user_id: &str,
    ) -> AppResult<()> {
        if channel.is_group_or_direct() {
            return Err(AppError::boxed(
                "CreateChannelWithUser",
                "api.channel.create_channel.direct_channel.app_error",
                None,
                String::new(),
                400,
            ));
        }
        if channel.is_board() {
            return Err(AppError::boxed(
                "CreateChannelWithUser",
                "app.channel.create_channel.board_type.app_error",
                None,
                "use CreateBoardChannel instead".to_owned(),
                400,
            ));
        }
        if channel.is_space() {
            return Err(AppError::boxed(
                "CreateChannelWithUser",
                "app.channel.create_channel.space_type.app_error",
                None,
                "use CreateChannel instead".to_owned(),
                400,
            ));
        }
        if channel.team_id.is_empty() {
            return Err(AppError::boxed(
                "CreateChannelWithUser",
                "app.channel.create_channel.no_team_id.app_error",
                None,
                String::new(),
                400,
            ));
        }

        let count = self
            .get_number_of_channels_on_team(&channel.team_id)
            .await?;
        let max = self.config().max_channels_per_team;
        if count + 1 > max {
            let params = std::collections::HashMap::from([(
                "MaxChannelsPerTeam".to_owned(),
                serde_json::Value::from(max),
            )]);
            return Err(AppError::boxed(
                "CreateChannelWithUser",
                "api.channel.create_channel.max_channel_limit.app_error",
                Some(params),
                String::new(),
                400,
            ));
        }

        channel.creator_id = user_id.to_owned();

        self.create_channel(ctx, channel, true).await?;

        self.add_channel_to_default_category(user_id, channel).await;

        // `GetUser`, then the creator's join notice. Both fail the route — after the channel,
        // its membership and its sidebar entry are written, which Go leaves standing.
        let user = self.get_user(user_id).await?;
        self.post_join_channel_message(ctx, &user, channel).await?;

        // `NewWebSocketEvent(channel_created, "", "", userID, nil, "")` — addressed to the
        // **user**, not to the channel or the team. A client learns about its own new channel and
        // nobody else is told, which is why a second member of a private channel sees nothing
        // until they are added.
        let mut message =
            WebSocketEvent::new(WEBSOCKET_EVENT_CHANNEL_CREATED, "", "", user_id, None, "");
        message.add("channel_id", serde_json::Value::String(channel.id.clone()));
        message.add(
            "team_id",
            serde_json::Value::String(channel.team_id.clone()),
        );
        self.publish(message).await;

        Ok(())
    }

    /// Port of `app.App.GetNumberOfChannelsOnTeam` (app/channel.go:3143).
    ///
    /// Go loads every channel of the team and takes `len`, so an **empty team is a 404**:
    /// `GetTeamChannels` answers `ErrNotFound` for zero rows and this maps it to
    /// `app.channel.get_channels.not_found.app_error`. Counting instead of listing keeps the
    /// query cheap and reproduces the 404 explicitly — a port that returned `Ok(0)` would let a
    /// create succeed where Go refuses it.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, count))]
    pub async fn get_number_of_channels_on_team(&self, team_id: &str) -> AppResult<i64> {
        let count = self
            .store()
            .channel()
            .count_team_channels(team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "team channel count failed");
                AppError::boxed(
                    "GetNumberOfChannelsOnTeam",
                    "app.channel.get_channels.get.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("count", count);

        if count == 0 {
            return Err(AppError::boxed(
                "GetNumberOfChannelsOnTeam",
                "app.channel.get_channels.not_found.app_error",
                None,
                String::new(),
                404,
            ));
        }

        Ok(count)
    }

    /// Port of `app.App.CreateChannel` (app/channel.go:238).
    ///
    /// # Three fields are trimmed and two of them are not the obvious ones
    ///
    /// `DisplayName`, `DefaultCategoryName` and `ManagedCategoryName` — **not** `Name`, `Header`
    /// or `Purpose`. A channel created with `"  spaced  "` as its name keeps both spaces and is
    /// then rejected by `IsValidChannelIdentifier`, while the same spacing in `display_name` is
    /// silently cleaned.
    ///
    /// # `add_member` makes the creator a channel **admin**
    ///
    /// `SchemeAdmin: true`, which no other membership write on any create path sets. It is
    /// invisible in this response — the body is the channel, not the member — and visible on the
    /// very next `GET /channels/{id}/members/{user_id}`, where `roles` reads
    /// `channel_user channel_admin`.
    ///
    /// # It returns `()` because Go's return value is its argument
    ///
    /// `Save` hands back the pointer it was given (`saveChannelT` returns `channel`), so Go's
    /// `sc` and the caller's `channel` are the same struct — which is why `CreateChannel` can
    /// blank `ManagedCategoryName` on `sc` and have the *caller's* channel change. Reproduced by
    /// mutating in place rather than by returning a copy.
    ///
    /// # The conflict is the caller's to interpret
    ///
    /// [`ChannelSave::Existing`] is a 400 here, because `POST /channels` is not idempotent. The
    /// group-channel path calls this same function and treats the identical outcome as a
    /// success, so the branch cannot live in the store.
    #[tracing::instrument(skip_all, fields(channel_type = %channel.channel_type, add_member))]
    pub async fn create_channel(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        channel: &mut Channel,
        add_member: bool,
    ) -> AppResult<()> {
        if channel.is_board() {
            return Err(AppError::boxed(
                "CreateChannel",
                "app.channel.create_channel.board_type.app_error",
                None,
                "use CreateBoardChannel instead".to_owned(),
                400,
            ));
        }
        if refuses_space(&self.config(), channel) {
            // A **403**, not the 400 every other refusal on this path answers.
            return Err(AppError::boxed(
                "CreateChannel",
                "app.channel.create_channel.spaces_not_enabled.app_error",
                None,
                String::new(),
                403,
            ));
        }

        channel.display_name = channel.display_name.trim().to_owned();
        channel.default_category_name = channel.default_category_name.trim().to_owned();
        channel.managed_category_name = channel.managed_category_name.trim().to_owned();

        let max = self.config().max_channels_per_team;
        match self.store().channel().save(channel, max).await {
            Ok(ChannelSave::Saved) => {}
            Ok(ChannelSave::Existing(_)) => {
                return Err(AppError::boxed(
                    "CreateChannel",
                    CHANNEL_EXISTS_ERROR,
                    None,
                    String::new(),
                    400,
                ));
            }
            Err(err) => {
                return Err(save_channel_error(
                    "CreateChannel",
                    "store.sql_channel.save.direct_channel.app_error",
                    err,
                ));
            }
        }

        if add_member {
            let user = self
                .store()
                .user()
                .get(&channel.creator_id)
                .await
                .map_err(|err| {
                    if err.is_not_found() {
                        AppError::boxed(
                            "CreateChannel",
                            MISSING_ACCOUNT_ERROR,
                            None,
                            String::new(),
                            404,
                        )
                    } else {
                        tracing::error!(error = %err, "creator lookup failed");
                        AppError::boxed(
                            "CreateChannel",
                            "app.user.get.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                })?;

            let member = ChannelMember {
                channel_id: channel.id.clone(),
                user_id: user.id.clone(),
                scheme_guest: user.is_guest(),
                scheme_user: !user.is_guest(),
                scheme_admin: true,
                notify_props: Some(get_default_channel_notify_props()),
                ..ChannelMember::default()
            };

            self.store()
                .channel()
                .save_member(member)
                .await
                .map_err(|err| save_member_error("CreateChannel", err))?;

            self.store()
                .channel_member_history()
                .log_join_event(&channel.creator_id, &channel.id, get_millis())
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "join history write failed");
                    AppError::boxed(
                        "CreateChannel",
                        "app.channel_member_history.log_join_event.internal_error",
                        None,
                        String::new(),
                        500,
                    )
                })?;

            self.hub()
                .invalidate_channel_members_for_user(&channel.creator_id);
        }

        // `SetChannelManagedCategory` needs `MinimumEnterpriseLicense` and the
        // `ManagedChannelCategories` feature flag. Unlicensed takes the `else` branch, which logs
        // and blanks the field *on the answer* — so a client that sent one gets it back empty.
        if !channel.managed_category_name.is_empty() {
            tracing::warn!("Managed category update ignored: feature not available");
            channel.managed_category_name = String::new();
        }

        if !channel.is_space() {
            self.channel_has_been_created(ctx, channel);
        }
        Ok(())
    }

    /// Port of `app.App.addChannelToDefaultCategory` (app/channel.go:4706), for a channel that
    /// has just been created.
    ///
    /// **Fire and forget.** Go logs every failure and returns nothing, so a sidebar that could
    /// not be written does not fail the create — and a caller cannot tell the two apart. That is
    /// reproduced: this returns `()`.
    ///
    /// The "already in a category" half of Go's logic is dead for a brand-new channel — nothing
    /// can reference an id the database learned about a millisecond ago — so only the
    /// find-or-create half is ported, and the doc comment is the record of why the rest is
    /// missing — along with the `SidebarCategoryDirectMessages` exclusion inside it, which only
    /// ever mattered for a channel that was already filed somewhere.
    ///
    /// Two details that decide whether the channel lands where Go puts it:
    ///
    /// - the match is **case-insensitive** (`strings.EqualFold`) and only against `custom`
    ///   categories, so a `default_category_name` of `"channels"` creates a *second*, custom
    ///   category rather than filing into the built-in one;
    /// - the channel is **prepended**, not appended, to an existing category.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, channel_id = %channel.id))]
    async fn add_channel_to_default_category(&self, user_id: &str, channel: &Channel) {
        if channel.default_category_name.is_empty()
            || !self.config().enable_channel_category_sorting
        {
            return;
        }

        let categories = match self
            .get_sidebar_categories_for_team_for_user(user_id, &channel.team_id)
            .await
        {
            Ok(categories) => categories,
            Err(err) => {
                tracing::error!(error = %err, user_id, team_id = %channel.team_id, "Failed to get sidebar categories");
                return;
            }
        };

        let target = categories
            .categories
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|category| {
                category.category.category_type == SIDEBAR_CATEGORY_CUSTOM
                    && category
                        .category
                        .display_name
                        .eq_ignore_ascii_case(&channel.default_category_name)
            })
            .cloned();

        match target {
            Some(mut target) => {
                let mut channels = target.channel_ids.take().unwrap_or_default();
                channels.insert(0, channel.id.clone());
                target.channel_ids = Some(channels);
                if let Err(err) = self
                    .update_sidebar_categories(user_id, &channel.team_id, &[target])
                    .await
                {
                    tracing::error!(error = %err, category_name = %channel.default_category_name, "Failed to update default category");
                }
            }
            None => {
                let new_category = SidebarCategoryWithChannels {
                    category: SidebarCategory {
                        user_id: user_id.to_owned(),
                        team_id: channel.team_id.clone(),
                        category_type: SIDEBAR_CATEGORY_CUSTOM.to_owned(),
                        display_name: channel.default_category_name.clone(),
                        sorting: SIDEBAR_CATEGORY_SORT_DEFAULT.to_owned(),
                        ..SidebarCategory::default()
                    },
                    channel_ids: Some(vec![channel.id.clone()]),
                };
                if let Err(err) = self
                    .create_sidebar_category(user_id, &channel.team_id, &new_category)
                    .await
                {
                    tracing::error!(error = %err, category_name = %channel.default_category_name, "Failed to create default category");
                }
            }
        }
    }

    /// Port of `app.App.GetOrCreateDirectChannel` (app/channel.go:351).
    ///
    /// # It is a *get* first, and that is what makes the route idempotent
    ///
    /// The lookup is by the deterministic name `GetDMNameFromIds` produces, so a second
    /// `POST /channels/direct` for the same pair returns the first one's channel — with a **201**,
    /// because the handler writes the status unconditionally. There is no 200 anywhere on this
    /// route.
    ///
    /// The store's own conflict is a *second* idempotency path, reached only when two creates
    /// race: `createDirectChannelWithUser` turns `ErrConflict("Channel")` into an app error whose
    /// id this function then recognises and swallows, returning the channel the loser's insert
    /// found. Both paths are ported and only the first is reachable in a test.
    ///
    /// # `RestrictDirectMessage = "team"` is forwarded
    ///
    /// That branch needs `IsBotExemptFromDMRestrictions` (a plugin decision) or
    /// `GetCommonTeamIDsForTwoUsers` (a store method this port does not have). The setting
    /// defaults to `"any"`, so the forward is unreachable on a stock server; it is a forward
    /// rather than a guess because refusing where Go allows would break every cross-team DM.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, other_user_id = %other_user_id, existed))]
    pub async fn get_or_create_direct_channel(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        user_id: &str,
        other_user_id: &str,
    ) -> AppResult<ChannelCreate> {
        if let Some(channel) = self.get_direct_channel(user_id, other_user_id).await? {
            tracing::Span::current().record("existed", true);
            return Ok(ChannelCreate::Created(Box::new(channel)));
        }
        tracing::Span::current().record("existed", false);

        if self.config().restrict_direct_message == crate::config::DIRECT_MESSAGE_TEAM {
            return Ok(ChannelCreate::Forward(
                "TeamSettings.RestrictDirectMessage is 'team'",
            ));
        }

        let channel = match self.create_direct_channel(user_id, other_user_id).await {
            Ok(ChannelCreate::Created(channel)) => *channel,
            Ok(forward @ ChannelCreate::Forward(_)) => return Ok(forward),
            Err(err) if err.id == CHANNEL_EXISTS_ERROR => {
                // Go returns `(channel, nil)` here, having carried the existing channel out
                // alongside the error. That value is the *loser's* view of the winner's row.
                match self.get_direct_channel(user_id, other_user_id).await? {
                    Some(channel) => return Ok(ChannelCreate::Created(Box::new(channel))),
                    None => return Err(err),
                }
            }
            Err(err) => return Err(err),
        };

        self.handle_creation_event(ctx, user_id, other_user_id, &channel)
            .await;
        Ok(ChannelCreate::Created(Box::new(channel)))
    }

    /// Port of `app.Server.getDirectChannel` (app/channel.go:4484).
    ///
    /// **A miss is `Ok(None)`, not an error** — Go swallows `ErrNotFound` and returns two nils,
    /// which is the only reason `GetOrCreateDirectChannel` can fall through to the create. Any
    /// other store failure is a 500 carrying `web.incoming_webhook.channel.app_error`, an id that
    /// has nothing to do with webhooks and is reproduced because a client may key off it.
    async fn get_direct_channel(
        &self,
        user_id: &str,
        other_user_id: &str,
    ) -> AppResult<Option<Channel>> {
        let name = get_dm_name_from_ids(user_id, other_user_id);
        match self.store().channel().get_by_name("", &name, true).await {
            Ok(channel) => Ok(Some(channel)),
            Err(err) if err.is_not_found() => Ok(None),
            Err(err) => {
                tracing::error!(error = %err, "direct channel lookup failed");
                Err(AppError::boxed(
                    "GetOrCreateDirectChannel",
                    "web.incoming_webhook.channel.app_error",
                    None,
                    String::new(),
                    500,
                ))
            }
        }
    }

    /// Port of `app.App.createDirectChannel` (app/channel.go:444) and
    /// `createDirectChannelWithUser` (:477).
    ///
    /// # The swap dance is not cosmetic
    ///
    /// `GetMany` returns two users in whatever order the database chose, and the channel's
    /// `CreatorId` is taken from whichever one is `user`. Go identifies them by id rather than by
    /// position for exactly that reason, and so does this.
    ///
    /// # A DM with yourself is legal
    ///
    /// `GetMany` de-duplicates, so one row comes back for `user_id == other_user_id` and Go
    /// appends it to itself to make two. The store then writes **one** membership.
    async fn create_direct_channel(
        &self,
        user_id: &str,
        other_user_id: &str,
    ) -> AppResult<ChannelCreate> {
        let ids = if user_id == other_user_id {
            vec![user_id.to_owned()]
        } else {
            vec![user_id.to_owned(), other_user_id.to_owned()]
        };
        // Go calls `Store().User().GetMany`; this is `GetProfileByIds` with no `Since`, which is
        // the same `usersQuery` over the same ids with an `ORDER BY Username` Go does not have.
        // The set is identical and the swap below identifies the two users by **id**, so the
        // extra ordering cannot change the answer — it only makes it deterministic.
        let mut users = self
            .store()
            .user()
            .get_profile_by_ids(&ids, 0)
            .await
            .map_err(|err| invalid_direct_user(&err.to_string()))?;

        if users.is_empty() {
            return Err(invalid_direct_user(&format!(
                "No users found for ids: {user_id}. {other_user_id}"
            )));
        }

        if user_id == other_user_id {
            let first = users[0].clone();
            users.push(first);
        }

        if users.len() != 2 {
            return Err(invalid_direct_user(&format!(
                "No users found for ids: {user_id}. {other_user_id}"
            )));
        }

        let (user, other_user) = if users[0].id == user_id {
            (users[0].clone(), users[1].clone())
        } else {
            (users[1].clone(), users[0].clone())
        };

        let shared = user.is_remote() || other_user.is_remote();
        if refuses_remote_participants(&self.config(), shared) {
            return Err(AppError::boxed(
                "createDirectChannelWithUser",
                "api.channel.create_channel.direct_channel.remote_restricted.app_error",
                None,
                String::new(),
                403,
            ));
        }

        // `SqlChannelStore.CreateDirectChannel` (channel_store.go:681) builds the channel and the
        // two memberships; it is inlined here because the store port's entry point is
        // `save_direct_channel`, one level down.
        // `ShareChannel` (app/channel.go:525) for a shared DM whose creator is local. With the sync
        // service running it writes `SharedChannels` and syncs; that is forwarded before any
        // write. Without it `ShareChannel` fails, Go logs it and still answers the channel.
        let share_fails = if shared && !user.is_remote() {
            if self.shared_channel_service_would_exist().await? {
                return Ok(ChannelCreate::Forward(
                    "a shared DM is shared through the shared-channel sync service",
                ));
            }
            true
        } else {
            false
        };

        let mut channel = Channel {
            display_name: String::new(),
            name: get_dm_name_from_ids(&other_user.id, &user.id),
            header: String::new(),
            channel_type: CHANNEL_TYPE_DIRECT.to_owned(),
            shared: Some(shared),
            creator_id: user.id.clone(),
            ..Channel::default()
        };

        let member1 = direct_member(&user);
        let member2 = direct_member(&other_user);

        match self
            .store()
            .channel()
            .save_direct_channel(&mut channel, member1, member2)
            .await
        {
            Ok(ChannelSave::Saved) => {}
            Ok(ChannelSave::Existing(_)) => {
                return Err(AppError::boxed(
                    "createDirectChannelWithUser",
                    CHANNEL_EXISTS_ERROR,
                    None,
                    String::new(),
                    400,
                ));
            }
            Err(err) if err.conflict_resource() == Some("ChannelMembers") => {
                return Err(save_member_error("createDirectChannelWithUser", err));
            }
            Err(err) => {
                return Err(save_channel_error(
                    "createDirectChannelWithUser",
                    "store.sql_channel.save_direct_channel.not_direct.app_error",
                    err,
                ));
            }
        }

        self.store()
            .channel_member_history()
            .log_join_event(&user.id, &channel.id, get_millis())
            .await
            .map_err(|err| log_join_failed("createDirectChannelWithUser", &err))?;
        if user.id != other_user.id {
            self.store()
                .channel_member_history()
                .log_join_event(&other_user.id, &channel.id, get_millis())
                .await
                .map_err(|err| log_join_failed("createDirectChannelWithUser", &err))?;
        }

        if share_fails {
            tracing::error!(channel_id = %channel.id, error = SHARED_CHANNELS_SERVICE_DISABLED, "Failed to share newly created direct channel");
        }

        Ok(ChannelCreate::Created(Box::new(channel)))
    }

    /// `getSharedChannelsService(false) != nil` (app/shared_channel.go:32): the service starts
    /// only under a licence that `HasSharedChannels` **and** with
    /// `ConnectedWorkspacesSettings.EnableSharedChannels` on (app/server.go:713-732). The same
    /// test `canUserDirectMessage` makes in `mm_api::connected_workspaces`.
    async fn shared_channel_service_would_exist(&self) -> AppResult<bool> {
        let licence = self.license().await?;
        Ok(
            licence.is_some_and(|l| l.has_shared_channels())
                && self.config().enable_shared_channels,
        )
    }

    /// Port of `app.App.handleCreationEvent` (app/channel.go:426).
    ///
    /// `direct_added` is addressed to the **channel**, so both members receive it — and the two
    /// cache invalidations above it are what make that work: a connected client's cached channel
    /// membership predates the new DM, and the hub filters a channel-addressed event against that
    /// cache. Dropping them delivers the event to nobody.
    ///
    /// `creator_id` is the *first* id the request listed, which need not be the session's user:
    /// the handler passes `userIds[0]` and `userIds[1]` positionally.
    async fn handle_creation_event(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        user_id: &str,
        other_user_id: &str,
        channel: &Channel,
    ) {
        self.hub().invalidate_channel_members_for_user(user_id);
        self.hub()
            .invalidate_channel_members_for_user(other_user_id);
        self.channel_has_been_created(ctx, channel);

        let mut message =
            WebSocketEvent::new(WEBSOCKET_EVENT_DIRECT_ADDED, "", &channel.id, "", None, "");
        message.add("creator_id", serde_json::Value::String(user_id.to_owned()));
        message.add(
            "teammate_id",
            serde_json::Value::String(other_user_id.to_owned()),
        );
        self.publish(message).await;
    }

    /// Port of `app.App.CreateGroupChannel` (app/channel.go:547) and `createGroupChannel` (:572).
    ///
    /// # One event per member, and the id list inside it is sorted
    ///
    /// `GetGroupNameFromUserIds` sorts Go's slice **in place**, and `CreateGroupChannel` then
    /// marshals that same slice into every event's `teammate_ids`. So the order on the wire is
    /// sorted even though the handler may have appended the session's own id last. The Rust
    /// helper does not mutate its argument, so the sort is explicit here — without it the field
    /// would carry the request's order and diverge for exactly the client that created the GM.
    ///
    /// # `teammate_ids` is a JSON array inside a JSON string
    ///
    /// `model.ArrayToJSON` produces `"[\"id\",…]"`, and `Add` stores that string. The same double
    /// encoding the sidebar and draft events use.
    #[tracing::instrument(skip_all, fields(creator_id = %creator_id, members = user_ids.len()))]
    pub async fn create_group_channel(
        &self,
        ctx: &crate::plugin_hooks::HookContext,
        user_ids: &[String],
        creator_id: &str,
    ) -> AppResult<ChannelCreate> {
        let channel = match self.create_group_channel_inner(user_ids, creator_id).await {
            Ok(ChannelCreate::Created(channel)) => *channel,
            Ok(forward @ ChannelCreate::Forward(_)) => return Ok(forward),
            Err(err) if err.id == CHANNEL_EXISTS_ERROR => {
                // The name is a hash of the membership, so the existing channel is *the* group
                // channel for this set. Go carries it out of the store; here it is re-read by the
                // same name, which is the only query that can find it.
                let name = get_group_name_from_user_ids(user_ids);
                return self
                    .store()
                    .channel()
                    .get_by_name("", &name, true)
                    .await
                    .map(|existing| ChannelCreate::Created(Box::new(existing)))
                    .map_err(|store_err| {
                        tracing::error!(error = %store_err, "existing group channel lookup failed");
                        err
                    });
            }
            Err(err) => return Err(err),
        };
        // The last thing Go's `createGroupChannel` does, before `CreateGroupChannel`'s events.
        self.channel_has_been_created(ctx, &channel);

        let mut sorted: Vec<String> = user_ids.to_vec();
        sorted.sort_unstable();
        let json_ids = serde_json::to_string(&sorted).unwrap_or_else(|_| "[]".to_owned());

        for user_id in &sorted {
            self.hub().invalidate_channel_members_for_user(user_id);

            let mut message = WebSocketEvent::new(
                WEBSOCKET_EVENT_GROUP_ADDED,
                "",
                &channel.id,
                user_id,
                None,
                "",
            );
            message.add("teammate_ids", serde_json::Value::String(json_ids.clone()));
            self.publish(message).await;
        }

        Ok(ChannelCreate::Created(Box::new(channel)))
    }

    /// The body of `createGroupChannel` (app/channel.go:572).
    ///
    /// `creator_id` decides one thing: whether a shared GM is shared (`ShareChannel` and the
    /// remote invites, :686), which happens only when the creator is among the participants and
    /// is local. The plugin API passes `""`, which is Go's `creator == nil`.
    async fn create_group_channel_inner(
        &self,
        user_ids: &[String],
        creator_id: &str,
    ) -> AppResult<ChannelCreate> {
        if user_ids.len() > CHANNEL_GROUP_MAX_USERS || user_ids.len() < CHANNEL_GROUP_MIN_USERS {
            return Err(AppError::boxed(
                "CreateGroupChannel",
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
                    "createGroupChannel",
                    "app.user.get_profiles.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        if users.len() != user_ids.len() {
            return Err(AppError::boxed(
                "CreateGroupChannel",
                "api.channel.create_group.bad_user.app_error",
                None,
                format!(
                    "user_ids={}",
                    serde_json::to_string(user_ids).unwrap_or_else(|_| "[]".to_owned())
                ),
                400,
            ));
        }

        // `remoteIDs` is a *set* of remote cluster ids, so `channelIsShared` is "at least one
        // participant is remote". With the shared-DM flag off every remote participant is then
        // refused.
        let channel_is_shared = users.iter().any(User::is_remote);
        if refuses_remote_participants(&self.config(), channel_is_shared) {
            return Err(AppError::boxed(
                "createGroupChannel",
                "api.channel.create_group.remote_restricted.app_error",
                None,
                String::new(),
                403,
            ));
        }

        // `channel.IsShared() && creator != nil && !creator.IsRemote()` (:686): `ShareChannel`
        // and one invite per remote. The running sync service is forwarded before any write;
        // without it `ShareChannel` fails, Go logs it, and sends no invite.
        let creator_local = users
            .iter()
            .any(|user| user.id == creator_id && !user.is_remote());
        let share_fails = if channel_is_shared && creator_local {
            if self.shared_channel_service_would_exist().await? {
                return Ok(ChannelCreate::Forward(
                    "a shared GM is shared through the shared-channel sync service",
                ));
            }
            true
        } else {
            false
        };

        let mut group = Channel {
            name: get_group_name_from_user_ids(user_ids),
            display_name: get_group_display_name_from_users(users.iter(), true),
            channel_type: CHANNEL_TYPE_GROUP.to_owned(),
            // `new(channelIsShared)` — a **pointer**, never nil, so a group channel's `shared` is
            // `false` on the wire where a fresh public channel's is `null`.
            shared: Some(channel_is_shared),
            ..Channel::default()
        };

        let max = self.config().max_channels_per_team;
        match self.store().channel().save(&mut group, max).await {
            Ok(ChannelSave::Saved) => {}
            Ok(ChannelSave::Existing(_)) => {
                return Err(AppError::boxed(
                    "CreateChannel",
                    CHANNEL_EXISTS_ERROR,
                    None,
                    String::new(),
                    400,
                ));
            }
            Err(err) => {
                return Err(save_channel_error(
                    "CreateChannel",
                    "store.sql_channel.save.direct_channel.app_error",
                    err,
                ));
            }
        }

        for user in &users {
            let member = ChannelMember {
                channel_id: group.id.clone(),
                user_id: user.id.clone(),
                notify_props: Some(get_default_channel_notify_props()),
                scheme_guest: user.is_guest(),
                scheme_user: !user.is_guest(),
                ..ChannelMember::default()
            };
            self.store()
                .channel()
                .save_member(member)
                .await
                .map_err(|err| save_member_error("createGroupChannel", err))?;
            self.store()
                .channel_member_history()
                .log_join_event(&user.id, &group.id, get_millis())
                .await
                .map_err(|err| log_join_failed("createGroupChannel", &err))?;
        }

        if share_fails {
            tracing::error!(channel_id = %group.id, error = SHARED_CHANNELS_SERVICE_DISABLED, "Failed to share newly created group channel");
        }

        Ok(ChannelCreate::Created(Box::new(group)))
    }
}

/// The membership `SqlChannelStore.CreateDirectChannel` (channel_store.go:706) builds.
///
/// **`scheme_admin` is left false**: neither party to a DM is a channel admin, unlike the creator
/// of a public or private channel. `channel_id` is filled in by the store once `PreSave` has
/// minted the channel's id.
fn direct_member(user: &User) -> ChannelMember {
    ChannelMember {
        user_id: user.id.clone(),
        notify_props: Some(get_default_channel_notify_props()),
        scheme_guest: user.is_guest(),
        scheme_user: !user.is_guest(),
        ..ChannelMember::default()
    }
}

/// `api.channel.create_direct_channel.invalid_user.app_error` at 400 — the one id both the
/// `GetMany` failure and the two "wrong number of users" branches answer with.
fn invalid_direct_user(details: &str) -> Box<AppError> {
    AppError::boxed(
        "CreateDirectChannel",
        "api.channel.create_direct_channel.invalid_user.app_error",
        None,
        details.to_owned(),
        400,
    )
}

fn log_join_failed(handler: &'static str, err: &StoreError) -> Box<AppError> {
    tracing::error!(error = %err, "join history write failed");
    AppError::boxed(
        handler,
        "app.channel_member_history.log_join_event.internal_error",
        None,
        String::new(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use mm_model::channel::CHANNEL_TYPE_SPACE;
    use mm_store::SqlStore;
    use sqlx::postgres::PgPoolOptions;

    fn unreachable_store() -> SqlStore {
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        SqlStore::from_pool(pool)
    }

    fn with_flags(edit: impl FnOnce(&mut mm_model::feature_flags::FeatureFlags)) -> Config {
        let mut config = Config::default();
        edit(&mut config.feature_flags);
        config
    }

    /// `createDirectChannelWithUser` (channel.go:478) and `createGroupChannel` (:603): a remote
    /// participant is refused only while `EnableSharedChannelsDMs` is off, and a channel with no
    /// remote participant is never refused.
    #[test]
    fn a_remote_participant_is_refused_only_with_the_shared_dm_flag_off() {
        let off = Config::default();
        let on = with_flags(|f| f.enable_shared_channels_dms = true);
        assert!(
            !off.feature_flags.enable_shared_channels_dms,
            "Go's default"
        );
        assert!(refuses_remote_participants(&off, true));
        assert!(!refuses_remote_participants(&off, false));
        assert!(!refuses_remote_participants(&on, true));
        assert!(!refuses_remote_participants(&on, false));
    }

    /// `CreateChannel` (channel.go:243): a space is a 403 with `EnableDocs` off; with it on the
    /// gate passes and the create goes on to the store — here unreachable, so a different error.
    #[tokio::test]
    async fn a_space_is_refused_only_with_docs_off() {
        let space = || Channel {
            channel_type: CHANNEL_TYPE_SPACE.to_owned(),
            display_name: "  Docs  ".to_owned(),
            ..Channel::default()
        };
        let create = |config: Config| async move {
            let app = App::with_config(unreachable_store(), config);
            let mut channel = space();
            let err = app
                .create_channel(
                    &crate::plugin_hooks::HookContext::default(),
                    &mut channel,
                    false,
                )
                .await
                .expect_err("either refused or the store is unreachable");
            (err, channel)
        };

        let (err, untouched) = create(Config::default()).await;
        assert_eq!(
            err.id,
            "app.channel.create_channel.spaces_not_enabled.app_error"
        );
        assert_eq!(err.status_code, 403);
        assert_eq!(
            untouched.display_name, "  Docs  ",
            "refused before the trim"
        );

        let (err, trimmed) = create(with_flags(|f| f.enable_docs = true)).await;
        assert_ne!(
            err.id,
            "app.channel.create_channel.spaces_not_enabled.app_error"
        );
        assert_eq!(trimmed.display_name, "Docs", "past the gate");

        let open = Channel {
            channel_type: mm_model::channel::CHANNEL_TYPE_OPEN.to_owned(),
            ..Channel::default()
        };
        assert!(!refuses_space(&Config::default(), &open));
    }

    /// `getSharedChannelsService(false) != nil`: a licence with shared channels **and**
    /// `ConnectedWorkspacesSettings.EnableSharedChannels`.
    #[tokio::test]
    async fn the_sync_service_needs_the_licence_and_the_setting() {
        let app = |sku: &str, enabled: bool| {
            App::with_config(
                unreachable_store(),
                Config {
                    enable_shared_channels: enabled,
                    ..crate::license::test_signing::licensed_config(sku)
                },
            )
        };
        assert!(
            app("professional", true)
                .shared_channel_service_would_exist()
                .await
                .unwrap()
        );
        assert!(
            !app("professional", false)
                .shared_channel_service_would_exist()
                .await
                .unwrap()
        );
        // Below professional, `HasSharedChannels` is the feature bit — which `SetDefaults` seeds
        // from `FutureFeatures`, so it has to be switched off explicitly.
        let without_feature = App::with_config(
            unreachable_store(),
            Config {
                enable_shared_channels: true,
                ..crate::license::test_signing::licensed_config_from(
                    r#"{"id":"mmrslicensedtestkey0000001","issued_at":1,"starts_at":1,"expires_at":4102444800000,"customer":{"id":"c","name":"n","email":"e","company":"co"},"features":{"users":10,"future_features":false,"shared_channels":false},"sku_name":"starter","sku_short_name":"starter"}"#,
                )
            },
        );
        assert!(
            !without_feature
                .shared_channel_service_would_exist()
                .await
                .unwrap()
        );
    }
}
