//! Port of the channel app-layer surface (channels/app/channel.go): `GetChannel`,
//! `GetChannelByName`, `GetChannelsForTeamForUser`, `GetChannelsForUser`, `GetChannelMember`,
//! `GetChannelUnread` and `FillInChannelsProps`.

use std::collections::HashMap;

use mm_model::channel::{Channel, ChannelSearchOpts};
use mm_model::channel_list::ChannelList;
use mm_model::channel_member::{
    CHANNEL_MARK_UNREAD_MENTION, ChannelMember, ChannelMembersWithTeamData, ChannelUnread,
};
use mm_model::post_list::PostList;
use mm_model::user::MARK_UNREAD_NOTIFY_PROP;
use mm_model::utils::{AppError, AppResult, get_preferred_timezone, remove_duplicate_strings};
use mm_store::ChannelStore;

use crate::App;

impl App {
    /// Port of `app.App.GetChannel` (channel.go:2225) and the `Server.getChannel` (:2274) it
    /// delegates to.
    ///
    /// The two error branches are not interchangeable, and `SessionHasPermissionToChannel` is why:
    /// it treats a **404** as "no such channel, deny quietly" and anything else as "the lookup
    /// broke, log it and deny" (authorization.go:107-111). Collapsing them into one status would
    /// make a database outage indistinguishable from a missing channel in the logs — the only
    /// place that distinction survives, since both answers deny.
    ///
    /// `channel_id` travels in `params` because Go puts it there (`errCtx`), and the i18n strings
    /// interpolate it.
    ///
    /// **Not ported:** `HydrateChannelPolicyActions`, which Go calls next and whose failure it only
    /// logs. See [D-141].
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    pub async fn get_channel(&self, channel_id: &str) -> AppResult<Channel> {
        self.store().channel().get(channel_id).await.map_err(|err| {
            let params = HashMap::from([(
                "channel_id".to_owned(),
                serde_json::Value::String(channel_id.to_owned()),
            )]);

            if err.is_not_found() {
                AppError::boxed(
                    "GetChannel",
                    "app.channel.get.existing.app_error",
                    Some(params),
                    String::new(),
                    404,
                )
            } else {
                tracing::error!(error = %err, "channel lookup failed");
                AppError::boxed(
                    "GetChannel",
                    "app.channel.get.find.app_error",
                    Some(params),
                    String::new(),
                    500,
                )
            }
        })
    }

    /// Port of `app.App.GetChannelMember` (channel.go:2258) and the `Server.getChannelMember`
    /// (:2262) it delegates to.
    ///
    /// Same two-branch shape as [`App::get_channel`], but note the **error ids are not the same
    /// pattern**: the miss is `app.channel.get_member.missing.app_error` — spelled out in
    /// `app/constants.go:6` as `MissingChannelMemberError` rather than inline — while the failure
    /// is `app.channel.get_member.app_error`. One is a suffix of the other with `missing.`
    /// inserted, which is easy to transcribe wrongly and impossible to notice from the outside,
    /// because both render as their own id until i18n runs.
    ///
    /// Neither branch carries `params`: Go passes `nil` here, unlike `GetChannel` which passes an
    /// `errCtx` with the channel id.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
    pub async fn get_channel_member(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> AppResult<ChannelMember> {
        self.store()
            .channel()
            .get_member(channel_id, user_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetChannelMember",
                        "app.channel.get_member.missing.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "channel member lookup failed");
                    AppError::boxed(
                        "GetChannelMember",
                        "app.channel.get_member.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
    }

    /// Port of `app.App.GetChannelUnread` (channel.go:2700).
    ///
    /// # Both branches carry the **same** error id
    ///
    /// Unlike [`App::get_channel`] and [`App::get_channel_member`], whose 404 and 500 are
    /// distinguishable ids, this function answers `app.channel.get_unread.app_error` either way
    /// and varies only the status. So a client that branches on `id` cannot tell a missing
    /// channel from a broken database here, and neither can a log reader who only has the id.
    /// Reproduced rather than improved: the id is on the wire.
    ///
    /// # The `mention` shortcut zeroes two of seven counters
    ///
    /// A member whose `mark_unread` notify prop is `mention` has asked not to be told about plain
    /// messages, so Go blanks `MsgCount` and `MsgCountRoot` — and **only** those. The three
    /// mention counts and `TeamId`/`ChannelId` survive, which is the point: a muted channel still
    /// reports the mentions that pierce the mute. Zeroing the mention counts too, or reading the
    /// prop as a mute of everything, silently loses a notification a client would have shown.
    ///
    /// A nil `NotifyProps` indexes to `""` in Go, which is not `mention`, so an absent map means
    /// the counts pass through — the same answer `Option::None` gives here.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
    pub async fn get_channel_unread(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> AppResult<ChannelUnread> {
        let mut unread = self
            .store()
            .channel()
            .get_channel_unread(channel_id, user_id)
            .await
            .map_err(|err| {
                let status = if err.is_not_found() {
                    404
                } else {
                    tracing::error!(error = %err, "channel unread lookup failed");
                    500
                };
                AppError::boxed(
                    "GetChannelUnread",
                    "app.channel.get_unread.app_error",
                    None,
                    String::new(),
                    status,
                )
            })?;

        apply_mark_unread_shortcut(&mut unread);

        Ok(unread)
    }

    /// Port of `app.App.GetChannelMembersPage` (channel.go:2578).
    ///
    /// Go builds `Offset: page * perPage, Limit: perPage` and hands the store its
    /// `ChannelMembersGetOptions`; the two `> 0` guards live **in the store**, so a
    /// `per_page = 0` reaches it as `Limit: 0` and means *unlimited* — see the store's doc.
    /// The multiplication wraps rather than panics, as Go's `int` product does; a wrapped
    /// (negative) offset then fails the store's `> 0` guard and reads from the start, which is
    /// also what Go's squirrel builder does with it.
    ///
    /// One branch, one id, 500-only: a channel with no members — or no channel at all — is an
    /// empty list, not a miss.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, page, per_page))]
    pub async fn get_channel_members_page(
        &self,
        channel_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<ChannelMember>> {
        self.store()
            .channel()
            .get_members(channel_id, page.wrapping_mul(per_page), per_page)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel members page lookup failed");
                AppError::boxed(
                    "GetChannelMembersPage",
                    "app.channel.get_members.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetChannelMembersByIds` (channel.go:2609).
    ///
    /// One branch, 500-only, with an id of its own —
    /// `app.channel.get_members_by_ids.app_error`, which is neither
    /// [`App::get_channel_members_page`]'s nor [`App::get_channel_members_for_user`]'s shared
    /// `app.channel.get_members.app_error`. Three sibling member lookups, three `where` values,
    /// two ids; a copy-paste that kept the shared one would be invisible until a client branched.
    ///
    /// No miss branch: an id list matching nothing is `[]` with a 200, not a 404.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, asked = user_ids.len(), count))]
    pub async fn get_channel_members_by_ids(
        &self,
        channel_id: &str,
        user_ids: &[String],
    ) -> AppResult<Vec<ChannelMember>> {
        let members = self
            .store()
            .channel()
            .get_members_by_ids(channel_id, user_ids)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel members by ids lookup failed");
                AppError::boxed(
                    "GetChannelMembersByIds",
                    "app.channel.get_members_by_ids.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("count", members.len());
        Ok(members)
    }

    /// Port of `app.App.GetChannelMembersForUser` (channel.go:2618).
    ///
    /// One branch, 500-only, and the id is **shared with [`App::get_channel_members_page`]**
    /// (`app.channel.get_members.app_error`) — only `where` tells the two apart on the wire. A
    /// user with no memberships in the team is `[]`, never a miss: the store builds
    /// `model.ChannelMembers{}` before appending, so the empty case encodes as an array.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, count))]
    pub async fn get_channel_members_for_user(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> AppResult<Vec<ChannelMember>> {
        let members = self
            .store()
            .channel()
            .get_members_for_user(team_id, user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel members for user lookup failed");
                AppError::boxed(
                    "GetChannelMembersForUser",
                    "app.channel.get_members.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("count", members.len());
        Ok(members)
    }

    /// Port of `app.App.GetChannelMemberCount` (channel.go:2664).
    ///
    /// One branch: the store's only failure mode is a broken query, so there is no 404 here — a
    /// channel id that matches nothing is a legitimate count of zero. Over REST that zero is
    /// unreachable: `getChannelStats`'s gate fetches the channel itself and a miss denies before
    /// any grant branch, the admin's included — **measured**, after a first draft of the parity
    /// suite asserted the opposite and both servers refused.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    pub async fn get_channel_member_count(&self, channel_id: &str) -> AppResult<i64> {
        self.store()
            .channel()
            .get_member_count(channel_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel member count failed");
                AppError::boxed(
                    "GetChannelMemberCount",
                    "app.channel.get_member_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetChannelFileCount` (channel.go:2673).
    ///
    /// The `where` Go passes is **`SqlChannelStore.GetFileCount`** — the store method's name, not
    /// this function's. A copy-paste in Go, reproduced because the string is on the wire when
    /// `EnableDeveloper` exposes it, and because "fix" and "drift" are indistinguishable later.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    pub async fn get_channel_file_count(&self, channel_id: &str) -> AppResult<i64> {
        self.store()
            .channel()
            .get_file_count(channel_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel file count failed");
                AppError::boxed(
                    "SqlChannelStore.GetFileCount",
                    "app.channel.get_file_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetChannelGuestCount` (channel.go:2682).
    ///
    /// Two transcription traps in one error, both Go's: the `where` is the store method's name
    /// (`SqlChannelStore.GetGuestCount`, like [`App::get_channel_file_count`]), and the id
    /// **reuses `app.channel.get_member_count.app_error`** — there is no `get_guest_count` id
    /// anywhere in Go. A reader tidying either one changes the wire.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    pub async fn get_channel_guest_count(&self, channel_id: &str) -> AppResult<i64> {
        self.store()
            .channel()
            .get_guest_count(channel_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel guest count failed");
                AppError::boxed(
                    "SqlChannelStore.GetGuestCount",
                    "app.channel.get_member_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetChannelPinnedPostCount` (channel.go:2691).
    ///
    /// The id is `app.channel.get_pinnedpost_count.app_error` — no underscore inside
    /// `pinnedpost`, the same missing underscore as the wire tag on
    /// [`mm_model::channel_stats::ChannelStats::pinned_post_count`].
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
    pub async fn get_channel_pinned_post_count(&self, channel_id: &str) -> AppResult<i64> {
        self.store()
            .channel()
            .get_pinned_post_count(channel_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel pinned post count failed");
                AppError::boxed(
                    "GetChannelPinnedPostCount",
                    "app.channel.get_pinnedpost_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetChannelsByNames` (channel.go:2350).
    ///
    /// One branch, one error id, no 404: a name that matches nothing is simply absent from the
    /// result, and only a broken query is an error. Go passes `allowFromCache = true`; this port
    /// has no channel-by-name cache, so the parameter does not exist — see the store method.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, names = channel_names.len()))]
    pub async fn get_channels_by_names(
        &self,
        channel_names: &[String],
        team_id: &str,
    ) -> AppResult<Vec<Channel>> {
        self.store()
            .channel()
            .get_by_names(team_id, channel_names)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channels-by-names lookup failed");
                AppError::boxed(
                    "GetChannelsByNames",
                    "app.channel.get_by_name.existing.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetChannelByName` (channel.go:2327).
    ///
    /// `includeDeleted` picks between two store methods in Go; here it is the flag the store's
    /// single `get_by_name` takes. The two error branches are the by-name ids — `missing` for a
    /// 404 and `existing` for a 500 — and, unlike `GetChannel`, neither carries `params`.
    ///
    /// **Not ported:** `HydrateChannelPolicyActions`, which follows the fetch and whose failure
    /// Go only logs. See [D-141].
    #[tracing::instrument(skip_all, fields(team_id = %team_id, name = %channel_name, include_deleted))]
    pub async fn get_channel_by_name(
        &self,
        channel_name: &str,
        team_id: &str,
        include_deleted: bool,
    ) -> AppResult<Channel> {
        self.store()
            .channel()
            .get_by_name(team_id, channel_name, include_deleted)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetChannelByName",
                        "app.channel.get_by_name.missing.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "channel-by-name lookup failed");
                    AppError::boxed(
                        "GetChannelByName",
                        "app.channel.get_by_name.existing.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
    }

    /// Port of `app.App.GetChannelMembersWithTeamDataForUserWithPagination` (channel.go:2640).
    ///
    /// `page == -1` selects the cursor walk and anything else the offset page — Go's
    /// `ChannelMemberCursor` carries both shapes in one struct and branches on the sentinel.
    ///
    /// # The `where` field names the *store* method, not this function
    ///
    /// Go assigns `method` from whichever branch it took and passes that to `NewAppError`, so a
    /// failure reports `GetMembersForUserWithCursorPagination` or `GetMembersForUserWithPagination`
    /// rather than the caller. Reproduced: `where` is not on the wire, but it is what a log
    /// reader uses to tell the two branches apart, which is the only reason Go bothers.
    ///
    /// # The 404 is a control-flow signal
    ///
    /// Only the cursor branch can raise it — its store call treats an empty page as
    /// `ErrNotFound` — and the streaming handler above reads that 404 as "the walk is done".
    /// The error id is `app.channel.get_member.missing.app_error`.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, page, per_page, found))]
    pub async fn get_channel_members_with_team_data_for_user_with_pagination(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
        from_channel_id: &str,
    ) -> AppResult<ChannelMembersWithTeamData> {
        let (result, method) = if page == -1 {
            (
                self.store()
                    .channel()
                    .get_members_for_user_with_cursor_pagination(user_id, per_page, from_channel_id)
                    .await,
                "GetMembersForUserWithCursorPagination",
            )
        } else {
            (
                self.store()
                    .channel()
                    .get_members_for_user_with_pagination(user_id, page, per_page)
                    .await,
                "GetMembersForUserWithPagination",
            )
        };

        let members = result.map_err(|err| {
            if err.is_not_found() {
                AppError::boxed(
                    method,
                    "app.channel.get_member.missing.app_error",
                    None,
                    String::new(),
                    404,
                )
            } else {
                tracing::error!(error = %err, "paginated channel-member lookup failed");
                AppError::boxed(
                    method,
                    "app.channel.get_members.app_error",
                    None,
                    String::new(),
                    500,
                )
            }
        })?;
        tracing::Span::current().record("found", members.len());
        Ok(members)
    }

    /// Port of `app.App.AutocompleteChannelsForSearch` (channel.go:3434).
    ///
    /// The thinnest of the three autocompletes: trim the term, one store call, one error id.
    /// **No `GetUser`** — so no guest branch, and an unknown user id is not an error here — and
    /// **no `FilterChannelListForUserVisibility`**, which its sibling calls and which is a no-op
    /// on this build anyway. `includeDeleted` is hardcoded `true` as it is there, so archived
    /// channels are listed.
    ///
    /// The error id is the same `app.channel.search.app_error` the sibling uses, but the `where`
    /// is this function's own name rather than the sibling's `AutocompleteChannels`.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, user_id = %user_id, found))]
    pub async fn autocomplete_channels_for_search(
        &self,
        team_id: &str,
        user_id: &str,
        term: &str,
    ) -> AppResult<ChannelList> {
        let term = term.trim();

        let channels = self
            .store()
            .channel()
            .autocomplete_in_team_for_search(team_id, user_id, term)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel search autocomplete failed");
                AppError::boxed(
                    "AutocompleteChannelsForSearch",
                    "app.channel.search.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("found", channels.0.len());
        Ok(channels)
    }

    /// Port of `app.App.AutocompleteChannelsForTeam` (channel.go:3400).
    ///
    /// `includeDeleted` is hardcoded to **true** there, so archived channels are in the answer;
    /// the term is trimmed of surrounding whitespace before it reaches the store.
    ///
    /// The user is fetched for one bit — `IsGuest()` — and its own error is returned unwrapped,
    /// so an unknown user id answers `GetUser`'s 404 rather than a search error.
    ///
    /// **`FilterChannelListForUserVisibility` is not ported.** It returns its input untouched
    /// unless `FeatureFlags.DiscoverableChannels` is on
    /// (app/channel_discoverable_visibility.go:182), and that flag is false at the pinned SHA —
    /// the same gate [D-153] records for `serveDiscoverableNonMember`. The store's
    /// `Discoverable = true` disjunct is still ported, because it is a column predicate rather
    /// than a feature-flagged code path.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, user_id = %user_id, found))]
    pub async fn autocomplete_channels_for_team(
        &self,
        team_id: &str,
        user_id: &str,
        term: &str,
    ) -> AppResult<ChannelList> {
        // `strings.TrimSpace` — Go trims Unicode whitespace, which is `str::trim`.
        let term = term.trim();

        let user = self.get_user(user_id).await?;

        let channels = self
            .store()
            .channel()
            .autocomplete_in_team(team_id, user_id, term, user.is_guest())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel autocomplete failed");
                // Go's `where` is `AutocompleteChannels`, not the function's own name.
                AppError::boxed(
                    "AutocompleteChannels",
                    "app.channel.search.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("found", channels.0.len());
        Ok(channels)
    }

    /// Port of `app.App.GetChannelByNameForTeamName` (channel.go:2358).
    ///
    /// [`Self::get_channel_by_name`] with the team resolved by **name** first, and the same two
    /// channel error ids after it. Two things about the team half are worth stating:
    ///
    /// - **Both team branches are 404**, including the default one. Go writes
    ///   `app.team.get_by_name.app_error` with `http.StatusNotFound` (channel.go:2368), so a
    ///   genuine database failure resolving the team answers 404 here where every sibling
    ///   answers 500. Reproduced by delegating to [`crate::App::get_team_by_name`], which
    ///   already carries that shape.
    /// - **`where` differs from Go's** — this delegates, so the team errors say `GetTeamByName`
    ///   where Go says `GetChannelByNameForTeamName`. `where` is not a field of the JSON error
    ///   body (`id`, `message`, `detailed_error`, `request_id`, `status_code`), so nothing on
    ///   the wire moves; duplicating the function to change an invisible string would not.
    ///
    /// The team is used only for its id. Its own permissions are checked by the handler against
    /// `channel.TeamId`, which for a DM or GM is the empty string and not this team's id.
    #[tracing::instrument(skip_all, fields(team_name = %team_name, name = %channel_name, include_deleted))]
    pub async fn get_channel_by_name_for_team_name(
        &self,
        channel_name: &str,
        team_name: &str,
        include_deleted: bool,
    ) -> AppResult<Channel> {
        let team = self.get_team_by_name(team_name).await?;
        self.get_channel_by_name(channel_name, &team.id, include_deleted)
            .await
    }

    /// Port of `app.App.GetChannelsForTeamForUser` (channel.go:2409) through the
    /// `Server.getChannelsForTeamForUser` (:2394) it delegates to.
    ///
    /// The 404 here is **reachable**: the store answers `ErrNotFound` for zero rows, so a user who
    /// is a member of no channel in the team — or a caller with `manage_system` asking about a
    /// team the target never joined — gets `app.channel.get_channels.not_found.app_error`, not an
    /// empty array. Note the Go `Where` is `GetChannelsForUser` in both branches, copied from the
    /// sibling function: reproduced, since `where` is on the wire as the error's first field.
    ///
    /// **Not ported:** `HydrateChannelsPolicyActions`, a no-op unless some channel in the list is
    /// policy-enforced, and whose failure Go only logs — see [D-141].
    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, count))]
    pub async fn get_channels_for_team_for_user(
        &self,
        team_id: &str,
        user_id: &str,
        opts: &ChannelSearchOpts,
    ) -> AppResult<ChannelList> {
        let channels = self
            .store()
            .channel()
            .get_channels(team_id, user_id, opts)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetChannelsForUser",
                        "app.channel.get_channels.not_found.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "channels-for-team-for-user lookup failed");
                    AppError::boxed(
                        "GetChannelsForUser",
                        "app.channel.get_channels.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;
        tracing::Span::current().record("count", channels.0.len());
        Ok(channels)
    }

    /// Port of `app.App.GetChannelsForUser` (channel.go:2429): one keyset page of the user's
    /// channels across every team, in id order.
    ///
    /// The 404 is **load-bearing** here, not merely reachable: `getChannelsForUser` streams the
    /// pages and stops on this error once `from_channel_id` is set, so it is the normal end of
    /// the loop whenever the total is a multiple of the page size — and the answer for a user
    /// with no channels at all, which the handler then writes *after* the `[` it has already
    /// sent (see `mm_api::channels::get_channels_for_user`). The `Where` is `GetChannelsForUser`
    /// — here it is actually this function's own name.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, from_channel_id = %from_channel_id, count))]
    pub async fn get_channels_for_user(
        &self,
        user_id: &str,
        include_deleted: bool,
        last_delete_at: i64,
        page_size: i64,
        from_channel_id: &str,
    ) -> AppResult<ChannelList> {
        let channels = self
            .store()
            .channel()
            .get_channels_by_user(
                user_id,
                include_deleted,
                last_delete_at,
                page_size,
                from_channel_id,
            )
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetChannelsForUser",
                        "app.channel.get_channels.not_found.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "channels-for-user lookup failed");
                    AppError::boxed(
                        "GetChannelsForUser",
                        "app.channel.get_channels.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;
        tracing::Span::current().record("count", channels.0.len());
        Ok(channels)
    }

    /// Port of `app.App.GetPublicChannelsForTeam` (channel.go:2526).
    ///
    /// **One error branch, not two.** Unlike its `GetDeletedChannels` sibling this function does
    /// not test for `ErrNotFound`: every failure is `app.channel.get_public_channels.get.app_error`
    /// with a 500, including the one an out-of-range `page` produces. `page * per_page` is
    /// computed in `int64` in Go and *wraps* on overflow, so a large enough page reaches the store
    /// as a negative offset and the query fails — measured as a 500 with this id, which is why the
    /// handler does not clamp it away.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, offset, limit, count))]
    pub async fn get_public_channels_for_team(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
    ) -> AppResult<ChannelList> {
        let channels = self
            .store()
            .channel()
            .get_public_channels_for_team(team_id, offset, limit)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "public-channels-for-team lookup failed");
                AppError::boxed(
                    "GetPublicChannelsForTeam",
                    "app.channel.get_public_channels.get.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("count", channels.0.len());
        Ok(channels)
    }

    /// Port of `app.App.GetChannelsMemberCount` (app/channel.go:2313).
    ///
    /// Two ids on the store's one error — 404 `…get_channels_member_count.existing.app_error`
    /// for a not-found, 500 `…find.app_error` otherwise — but the store has **no not-found
    /// branch**: an id nothing matches is a `0` in the map, not an error. The 404 is dead code
    /// in Go too, and reproduced for the same reason its neighbours are.
    #[tracing::instrument(skip_all, fields(asked = channel_ids.len()))]
    pub async fn get_channels_member_count(
        &self,
        channel_ids: &[String],
    ) -> AppResult<std::collections::BTreeMap<String, i64>> {
        self.store()
            .channel()
            .get_channels_member_count(channel_ids)
            .await
            .map_err(|err| {
                let not_found = err.is_not_found();
                tracing::error!(error = %err, "channel member counts failed");
                if not_found {
                    AppError::boxed(
                        "GetChannelsMemberCount",
                        "app.channel.get_channels_member_count.existing.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    AppError::boxed(
                        "GetChannelsMemberCount",
                        "app.channel.get_channels_member_count.find.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
    }

    /// Port of `app.App.GetChannels` (channel.go:2289) — the plural of `GetChannel`, over an
    /// id list.
    ///
    /// Both error ids say **`GetChannel`**, singular, in the `where` field: Go passes the
    /// singular name to `NewAppError` in the plural function. Kept, because `where` is the one
    /// field of an `AppError` a client can see change without a translation file.
    ///
    /// `HydrateChannelsPolicyActions` is not ported. It fills `PolicyActions` on channels whose
    /// `PolicyEnforced` is true, and Go *logs and continues* when it fails rather than
    /// propagating — so its absence changes one unselected field on a channel this deployment
    /// cannot create, and never changes the status.
    #[tracing::instrument(skip_all, fields(asked = channel_ids.len(), count))]
    pub async fn get_channels(&self, channel_ids: &[String]) -> AppResult<Vec<Channel>> {
        let channels = self
            .store()
            .channel()
            .get_many(channel_ids)
            .await
            .map_err(|err| {
                let not_found = err.is_not_found();
                tracing::error!(error = %err, "channels-by-ids lookup failed");
                if not_found {
                    AppError::boxed(
                        "GetChannel",
                        "app.channel.get.existing.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    AppError::boxed(
                        "GetChannel",
                        "app.channel.get.find.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;
        tracing::Span::current().record("count", channels.len());
        Ok(channels)
    }

    /// Port of `app.App.GetPublicChannelsByIdsForTeam` (channel.go:2511).
    ///
    /// **The one two-branch member of this family.** Its siblings map every store failure to a
    /// 500; this one splits on `store.ErrNotFound` and answers **404**
    /// (`app.channel.get_channels_by_ids.not_found.app_error`) against the 500's
    /// `app.channel.get_channels_by_ids.get.app_error` — the two ids differ by one infix, and
    /// the store raises the miss for *zero rows*, not for a missing team. So a request naming
    /// only private, archived, or other-team channels is a 404 with no channel involved, and a
    /// non-existent team id is a 404 for the same reason rather than as a team lookup.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, asked = channel_ids.len(), count))]
    pub async fn get_public_channels_by_ids_for_team(
        &self,
        team_id: &str,
        channel_ids: &[String],
    ) -> AppResult<ChannelList> {
        let channels = self
            .store()
            .channel()
            .get_public_channels_by_ids_for_team(team_id, channel_ids)
            .await
            .map_err(|err| {
                let not_found = err.is_not_found();
                tracing::error!(error = %err, "public-channels-by-ids lookup failed");
                if not_found {
                    AppError::boxed(
                        "GetPublicChannelsByIdsForTeam",
                        "app.channel.get_channels_by_ids.not_found.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    AppError::boxed(
                        "GetPublicChannelsByIdsForTeam",
                        "app.channel.get_channels_by_ids.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;
        tracing::Span::current().record("count", channels.0.len());
        Ok(channels)
    }

    /// Port of `app.App.GetPrivateChannelsForTeam` (channel.go:2535).
    ///
    /// The public sibling with one string changed. The id is
    /// `app.channel.get_private_channels.get.app_error` and the `where` is
    /// `GetPrivateChannelsForTeam`; both are on the wire, and a copy-paste that kept the public
    /// one would be invisible until a client branched on the id.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, offset, limit, count))]
    pub async fn get_private_channels_for_team(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
    ) -> AppResult<ChannelList> {
        let channels = self
            .store()
            .channel()
            .get_private_channels_for_team(team_id, offset, limit)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "private-channels-for-team lookup failed");
                AppError::boxed(
                    "GetPrivateChannelsForTeam",
                    "app.channel.get_private_channels.get.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("count", channels.0.len());
        Ok(channels)
    }

    /// Port of `app.App.GetDeletedChannels` (channel.go:2488).
    ///
    /// **The 404 branch is dead and is ported anyway.** Go maps `store.ErrNotFound` to
    /// `app.channel.get_deleted.missing.app_error`, but the store reaches that only through
    /// `err == sql.ErrNoRows`, which `sqlx.Select` into a slice never returns — zero archived
    /// channels is `200 []` on the running Go server, measured. Keeping the branch costs nothing
    /// and means a future `GetDeleted` that *can* answer `NotFound` already carries Go's id;
    /// deleting it would silently promote that case to a 500.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, offset, limit, count))]
    pub async fn get_deleted_channels(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
        user_id: &str,
        skip_team_membership_check: bool,
    ) -> AppResult<ChannelList> {
        let channels = self
            .store()
            .channel()
            .get_deleted(team_id, offset, limit, user_id, skip_team_membership_check)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetDeletedChannels",
                        "app.channel.get_deleted.missing.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "deleted-channels lookup failed");
                    AppError::boxed(
                        "GetDeletedChannels",
                        "app.channel.get_deleted.existing.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;
        tracing::Span::current().record("count", channels.0.len());
        Ok(channels)
    }

    /// Port of `app.Server.getChannelMemberLastViewedAt` (app/channel.go:2563).
    ///
    /// # Two not-found ids for one missing row, and this is the other one
    ///
    /// A missing membership here is `api.channel.get_channel_member.missing.app_error` (404) —
    /// `MissingChannelMemberError`, the same constant [`App::get_channel_member`] uses. What is
    /// **not** shared is the 500: this one is `app.channel.get_member.app_error` where
    /// `GetChannelMember`'s is the same string, so the two really do agree on both ids and
    /// differ only in `Where`, which is `json:"-"`.
    ///
    /// A `LastViewedAt` of zero is a successful read, not an error: the caller treats it as
    /// "nothing is unread" and answers an empty list.
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id, user_id = %user_id))]
    pub async fn get_channel_member_last_viewed_at(
        &self,
        channel_id: &str,
        user_id: &str,
    ) -> AppResult<i64> {
        self.store()
            .channel()
            .get_member_last_viewed_at(channel_id, user_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "getChannelMemberLastViewedAt",
                        // `app.MissingChannelMemberError` (app/constants.go:6) — the same
                        // constant `GetChannelMember` uses, spelled out for the reason that
                        // function's doc comment gives.
                        "app.channel.get_member.missing.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "lastViewedAt lookup failed");
                    AppError::boxed(
                        "getChannelMemberLastViewedAt",
                        "app.channel.get_member.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
    }

    /// Port of `app.App.GetChannelMembersTimezones` (app/channel.go:2592).
    ///
    /// # Three transformations, and the order of the first two is on the wire
    ///
    /// 1. **Drop** any member whose `automaticTimezone` *and* `manualTimezone` are both empty.
    ///    Note the test is on those two fields only — a member with
    ///    `useAutomaticTimezone: "true"` and an empty `automaticTimezone` but a non-empty
    ///    `manualTimezone` survives this filter and then contributes the **empty string**,
    ///    because step 2 reads the automatic one.
    /// 2. **`GetPreferredTimezone`**: the automatic one when `useAutomaticTimezone` is the
    ///    string `"true"`, the manual one otherwise. The flag is a *string* in the map, so
    ///    anything but the exact five characters `true` means manual.
    /// 3. **`RemoveDuplicateStrings`**, which **sorts** before deduplicating — so the response is
    ///    alphabetical, not membership order, and that is the only thing giving this route a
    ///    stable order at all (the query has no `ORDER BY`).
    ///
    /// An empty result stays a **nil** slice: Go declares `var timezones []string` and appends,
    /// and `RemoveDuplicateStrings` of a nil slice is nil. The handler's `ArrayToJSON` renders
    /// that as `null`.
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id, count))]
    pub async fn get_channel_members_timezones(&self, channel_id: &str) -> AppResult<Vec<String>> {
        let members = self
            .store()
            .channel()
            .get_channel_members_timezones(channel_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "member timezone lookup failed");
                AppError::boxed(
                    "GetChannelMembersTimezones",
                    "app.channel.get_members.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let mut timezones: Vec<String> = Vec::new();
        for member in &members {
            let automatic = member.get("automaticTimezone").map(String::as_str);
            let manual = member.get("manualTimezone").map(String::as_str);
            if automatic.unwrap_or_default().is_empty() && manual.unwrap_or_default().is_empty() {
                continue;
            }
            timezones.push(get_preferred_timezone(member).to_owned());
        }
        remove_duplicate_strings(&mut timezones);
        tracing::Span::current().record("count", timezones.len());

        Ok(timezones)
    }

    /// Port of `app.App.GetPinnedPosts` (app/channel.go:3992).
    ///
    /// One store call, one error id, and **no not-found branch**: a channel id that names
    /// nothing is a successful read of zero rows. The 404 a client sees for a bad channel comes
    /// from `getPinnedPosts`'s own `GetChannel` call, two lines earlier in the handler.
    ///
    /// `filterInaccessiblePosts` sits between the read and the return in Go. It exits
    /// immediately without a licence carrying a `PostHistory` limit, so it cannot change the
    /// list on this deployment and is not reproduced — the same call, and the same reasoning, as
    /// in `mm_api::posts::get_post_thread`.
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id))]
    pub async fn get_pinned_posts(&self, channel_id: &str) -> AppResult<PostList> {
        self.store()
            .channel()
            .get_pinned_posts(channel_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "pinned post lookup failed");
                AppError::boxed(
                    "GetPinnedPosts",
                    "app.channel.pinned_posts.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.FillInChannelProps` (channel.go:4091): the one-element case of
    /// [`App::fill_in_channels_props`], which is exactly how Go defines it.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id))]
    pub async fn fill_in_channel_props(&self, channel: &mut Channel) -> AppResult<()> {
        self.fill_in_channels_props(std::slice::from_mut(channel))
            .await
    }

    /// Port of `app.App.FillInChannelsProps` (channel.go:4095).
    ///
    /// The prop is **written or deleted, never left stale**, and the delete has a guard:
    ///
    /// - A header that mentions at least one *open, living, same-team* channel gets
    ///   `props.channel_mentions = {name: {"display_name": …}, …}`.
    /// - A header whose mentions all miss (no such channel, archived, private, wrong team) does
    ///   not merely skip the write — it **removes** a `channel_mentions` prop that is already
    ///   there, because the header may have changed since the prop was computed.
    /// - A header with no `~mention` at all skips *everything*, the delete included — **provided
    ///   no other channel of the same team has one.** Go's `len(allChannelMentionNames) > 0`
    ///   guard (channel.go:4109) is per *team group*, not per channel: one mention anywhere in
    ///   the team's group runs the write-or-delete loop over every channel in it, and a channel
    ///   with an empty header then takes the delete branch. Through these routes that branch is a
    ///   no-op (the store never selects `Props`), so the distinction is not on the wire today.
    ///
    /// Channels are grouped by team and each team's mentions are looked up **once**; a DM or GM
    /// has `TeamId == ""`, which [`mm_store::channel_store::get_by_names`] turns into "every
    /// team", exactly as Go's omitted predicate does. Only `Type == "O"` mentions render; a
    /// private channel's existence is not leaked into a prop anyone in the channel can read.
    #[tracing::instrument(skip_all, fields(channels = channels.len()))]
    pub async fn fill_in_channels_props(&self, channels: &mut [Channel]) -> AppResult<()> {
        // Go groups with a map keyed by team; iteration order over a map is unspecified there
        // and irrelevant here, since each group is independent. A `BTreeMap` keeps the lookups
        // in a stable order for the logs.
        let mut indices_by_team: std::collections::BTreeMap<&str, Vec<usize>> =
            std::collections::BTreeMap::new();
        let mentions_by_index: Vec<Vec<String>> = channels
            .iter()
            .map(|c| mm_model::channel_mentions::channel_mentions(&c.header))
            .collect();
        for (index, channel) in channels.iter().enumerate() {
            indices_by_team
                .entry(channel.team_id.as_str())
                .or_default()
                .push(index);
        }
        // The borrow of `channels` through the keys ends here; the group list is owned below.
        let groups: Vec<(String, Vec<usize>)> = indices_by_team
            .into_iter()
            .map(|(team, indices)| (team.to_owned(), indices))
            .collect();

        for (team_id, indices) in groups {
            let mut all_mentions: Vec<String> = Vec::new();
            for &index in &indices {
                for mention in &mentions_by_index[index] {
                    if !all_mentions.contains(mention) {
                        all_mentions.push(mention.clone());
                    }
                }
            }
            if all_mentions.is_empty() {
                continue;
            }

            let mentioned = self.get_channels_by_names(&all_mentions, &team_id).await?;
            for &index in &indices {
                apply_channel_mentions_prop(
                    &mut channels[index],
                    &mentions_by_index[index],
                    &mentioned,
                );
            }
        }
        Ok(())
    }
}

/// The prop half of `FillInChannelsProps` (channel.go:4126-4147), lifted out of the store call so
/// its branches can be pinned without a database — the same reason `apply_mark_unread_shortcut`
/// exists below.
///
/// Only **open** channels render into the prop; a `~mention` of a private channel that the
/// lookup returned (it is in `messageChannelTypes`) is dropped *here*, so a private channel's
/// display name never leaks into a prop every channel reader can see. The key is the mentioned
/// channel's `Name` as the database has it, not the mention text — the two are equal by the map
/// lookup, but the distinction says which one is authoritative.
///
/// The `else` branch deletes a stale prop rather than leaving it. Through `getChannel` it is
/// dead code — the store never selects `Props`, so `channel.props` is `None` on entry — but Go
/// keeps it for callers that pass a hydrated channel, and dropping it would change any such
/// future call site silently. Same shape as the unreachable type filter in [D-151].
fn apply_channel_mentions_prop(channel: &mut Channel, mentions: &[String], mentioned: &[Channel]) {
    let by_name: HashMap<&str, &Channel> = mentioned.iter().map(|c| (c.name.as_str(), c)).collect();

    // `serde_json::Map` is a BTreeMap, so the keys serialise sorted — the same order Go's
    // `encoding/json` gives a `map[string]any`.
    let mut props = mm_model::utils::StringInterface::new();
    for mention in mentions {
        if let Some(mentioned) = by_name.get(mention.as_str()) {
            if mentioned.channel_type == mm_model::channel::CHANNEL_TYPE_OPEN {
                props.insert(
                    mentioned.name.clone(),
                    serde_json::json!({ "display_name": mentioned.display_name }),
                );
            }
        }
    }

    if !props.is_empty() {
        channel.add_prop("channel_mentions", serde_json::Value::Object(props));
    } else if let Some(existing) = channel.props.as_mut() {
        existing.remove("channel_mentions");
    }
}

/// Go's `if channelUnread.NotifyProps[MarkUnreadNotifyProp] == ChannelMarkUnreadMention`
/// (channel.go:2712), lifted out of the handler so it can be pinned without a database.
///
/// The store call above is the only other thing in `get_channel_unread`, and it needs Postgres —
/// so an inline branch here would be exercised solely by the cross-server suite, which cannot
/// distinguish "the counts were zero anyway" from "the shortcut fired". This is the same reason
/// `validate_ids` exists in `mm-api/src/channels.rs`.
fn apply_mark_unread_shortcut(unread: &mut ChannelUnread) {
    // Go indexes a `StringMap` directly, so a nil map and a missing key both yield `""` — which
    // is not `mention`, and the counts pass through. `Option` reproduces that, it does not add a
    // case.
    if unread
        .notify_props
        .as_ref()
        .and_then(|props| props.get(MARK_UNREAD_NOTIFY_PROP))
        .map(String::as_str)
        == Some(CHANNEL_MARK_UNREAD_MENTION)
    {
        unread.msg_count = 0;
        unread.msg_count_root = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_store::SqlStore;
    use sqlx::postgres::PgPoolOptions;

    /// An `App` pointed at a database that cannot be reached; `connect_lazy` defers the attempt to
    /// first use, so constructing it never fails and any store call does.
    fn unreachable_app() -> App {
        // `acquire_timeout` is set because sqlx's default is **30 seconds**, and the connection
        // to :1 is refused instantly but retried until that window expires. Six tests wearing that
        // default cost 90 seconds of every `cargo test -p mm-app`, and 6 minutes under
        // `--test-threads=1`. The error a caller sees is `PoolTimedOut` either way, so nothing
        // under test changes — only how long we wait to see it.
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        App::new(SqlStore::from_pool(pool))
    }

    fn unread() -> ChannelUnread {
        ChannelUnread {
            team_id: "tttttttttttttttttttttttttt".to_owned(),
            channel_id: "cccccccccccccccccccccccccc".to_owned(),
            msg_count: 17,
            msg_count_root: 11,
            mention_count: 3,
            mention_count_root: 2,
            urgent_mention_count: 1,
            notify_props: None,
        }
    }

    fn with_mark_unread(value: &str) -> ChannelUnread {
        let mut u = unread();
        let mut props = mm_model::utils::StringMap::new();
        props.insert(MARK_UNREAD_NOTIFY_PROP.to_owned(), value.to_owned());
        u.notify_props = Some(props);
        u
    }

    /// The store's 404 and its failure share one error id here, unlike every other function in
    /// this file — so the **status** is the only thing separating them, and a test that checked
    /// the id alone would pass for a port that always answered 500.
    #[tokio::test]
    async fn an_unread_lookup_failure_is_a_500_with_the_shared_error_id() {
        let err = unreachable_app()
            .get_channel_unread("cccccccccccccccccccccccccc", "uuuuuuuuuuuuuuuuuuuuuuuuuu")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.channel.get_unread.app_error");
        assert!(
            err.params.is_none(),
            "Go passes nil params in both branches"
        );
        assert_eq!(
            err.id, "app.channel.get_unread.app_error",
            "the 404 branch uses this same id — see the doc comment"
        );
    }

    /// `mention` blanks the two message counts and **nothing else**. The three mention counts
    /// surviving is the point of the setting, not an oversight.
    #[test]
    fn mark_unread_mention_zeroes_only_the_two_message_counts() {
        let mut u = with_mark_unread(mm_model::channel_member::CHANNEL_MARK_UNREAD_MENTION);
        apply_mark_unread_shortcut(&mut u);

        assert_eq!(u.msg_count, 0);
        assert_eq!(u.msg_count_root, 0);
        assert_eq!(u.mention_count, 3, "mentions pierce the mute");
        assert_eq!(u.mention_count_root, 2);
        assert_eq!(u.urgent_mention_count, 1);
        assert_eq!(u.team_id, "tttttttttttttttttttttttttt");
        assert_eq!(u.channel_id, "cccccccccccccccccccccccccc");
    }

    /// Every other value of the prop — including the explicit `all`, an unrecognised string, an
    /// empty string, a missing key and a missing map — leaves the counts alone. Go's comparison
    /// is equality against one constant, not a "is it muted" predicate.
    #[test]
    fn anything_other_than_mention_leaves_the_counts_alone() {
        let mut cases = vec![
            unread(),
            with_mark_unread(mm_model::channel_member::CHANNEL_MARK_UNREAD_ALL),
            with_mark_unread(""),
            with_mark_unread("Mention"),
            with_mark_unread("mention "),
            with_mark_unread("mentions"),
        ];
        // A present-but-unrelated key: the map exists, `mark_unread` does not.
        let mut other_key = unread();
        let mut props = mm_model::utils::StringMap::new();
        props.insert("desktop".to_owned(), "mention".to_owned());
        other_key.notify_props = Some(props);
        cases.push(other_key);
        // An empty map, which is a different row from a NULL one ([D-135]).
        let mut empty = unread();
        empty.notify_props = Some(mm_model::utils::StringMap::new());
        cases.push(empty);

        for case in &mut cases {
            let before = case.clone();
            apply_mark_unread_shortcut(case);
            assert_eq!(
                *case, before,
                "{:?} must pass through untouched",
                before.notify_props
            );
        }
    }

    /// The prop this reads is `mark_unread`, spelled as `model.MarkUnreadNotifyProp`. A shortcut
    /// keyed on any other name would silently never fire, and the counts would look correct.
    #[test]
    fn the_prop_name_and_value_are_gos_constants() {
        assert_eq!(MARK_UNREAD_NOTIFY_PROP, "mark_unread");
        assert_eq!(CHANNEL_MARK_UNREAD_MENTION, "mention");
    }

    /// A broken store is a **500**, not a 404. The distinction is the whole reason both branches
    /// exist: `SessionHasPermissionToChannel` logs one and not the other.
    #[tokio::test]
    async fn a_store_failure_is_a_500_not_a_missing_channel() {
        let err = unreachable_app()
            .get_channel("cccccccccccccccccccccccccc")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.channel.get.find.app_error");
    }

    /// A broken store is a 500 for the member lookup too, with the **non-missing** id.
    #[tokio::test]
    async fn a_member_lookup_failure_is_a_500_with_the_plain_error_id() {
        let err = unreachable_app()
            .get_channel_member("cccccccccccccccccccccccccc", "uuuuuuuuuuuuuuuuuuuuuuuuuu")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.channel.get_member.app_error");
        assert!(
            err.params.is_none(),
            "Go passes nil params here, unlike GetChannel"
        );
    }

    /// The two member error ids differ only by an inserted `missing.`, and the 404 is the one a
    /// client branches on. Pinned so a transcription slip fails a test rather than a client.
    #[test]
    fn the_two_member_error_ids_are_the_ones_go_uses() {
        assert_eq!(
            mm_model::utils::AppError::new(
                "GetChannelMember",
                "app.channel.get_member.missing.app_error",
                None,
                String::new(),
                404
            )
            .id,
            "app.channel.get_member.missing.app_error",
            "app/constants.go:6"
        );
    }

    /// The four count wrappers have one branch each — a broken store is a 500, and there is no
    /// 404 anywhere in them. Each is pinned with its id **and** its `where`, because two of the
    /// four carry the store method's name rather than their own, and one reuses another's id;
    /// all three quirks are Go's and all three are exactly what a tidy-minded port loses.
    #[tokio::test]
    async fn the_count_wrappers_carry_gos_exact_error_identities() {
        let app = unreachable_app();
        let channel = "cccccccccccccccccccccccccc";

        let member = app.get_channel_member_count(channel).await.unwrap_err();
        assert_eq!(member.status_code, 500);
        assert_eq!(member.id, "app.channel.get_member_count.app_error");
        assert_eq!(member.where_, "GetChannelMemberCount");

        let file = app.get_channel_file_count(channel).await.unwrap_err();
        assert_eq!(file.status_code, 500);
        assert_eq!(file.id, "app.channel.get_file_count.app_error");
        assert_eq!(
            file.where_, "SqlChannelStore.GetFileCount",
            "Go passes the store method's name, not GetChannelFileCount (channel.go:2676)"
        );

        let guest = app.get_channel_guest_count(channel).await.unwrap_err();
        assert_eq!(guest.status_code, 500);
        assert_eq!(
            guest.id, member.id,
            "there is no get_guest_count id in Go — the member-count id is reused (channel.go:2685)"
        );
        assert_eq!(guest.where_, "SqlChannelStore.GetGuestCount");

        let pinned = app
            .get_channel_pinned_post_count(channel)
            .await
            .unwrap_err();
        assert_eq!(pinned.status_code, 500);
        assert_eq!(
            pinned.id, "app.channel.get_pinnedpost_count.app_error",
            "no underscore inside pinnedpost — the same missing underscore as the wire tag"
        );
        assert_eq!(pinned.where_, "GetChannelPinnedPostCount");

        for err in [&member, &file, &guest, &pinned] {
            assert!(err.params.is_none(), "Go passes nil params in all four");
        }
    }

    /// The id travels in `params`, as Go's `errCtx` does — the i18n string interpolates it.
    #[tokio::test]
    async fn the_channel_id_is_carried_in_params() {
        let err = unreachable_app()
            .get_channel("cccccccccccccccccccccccccc")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(
            err.params
                .as_ref()
                .and_then(|p| p.get("channel_id"))
                .and_then(serde_json::Value::as_str),
            Some("cccccccccccccccccccccccccc")
        );
    }

    /// A bare-bones channel for the `channel_mentions` prop tests. Only the four fields the
    /// function reads (`header`, `name`, `channel_type`, `display_name`) and the one it writes
    /// (`props`) matter; everything else stays zero.
    fn bare_channel(name: &str, channel_type: &str, display_name: &str) -> Channel {
        Channel {
            id: String::new(),
            create_at: 0,
            update_at: 0,
            delete_at: 0,
            team_id: String::new(),
            channel_type: channel_type.to_owned(),
            display_name: display_name.to_owned(),
            name: name.to_owned(),
            header: String::new(),
            purpose: String::new(),
            last_post_at: 0,
            total_msg_count: 0,
            extra_update_at: 0,
            creator_id: String::new(),
            scheme_id: None,
            props: None,
            group_constrained: None,
            auto_translation: false,
            shared: None,
            total_msg_count_root: 0,
            policy_id: None,
            last_root_post_at: 0,
            banner_info: None,
            policy_enforced: false,
            policy_actions: None,
            policy_is_active: false,
            default_category_name: String::new(),
            managed_category_name: String::new(),
            discoverable: false,
        }
    }

    /// Only `Type == "O"` renders into the prop. The private channel came back from the lookup —
    /// `messageChannelTypes` includes `P` — and is dropped *here*, so its display name never
    /// reaches a prop every channel reader can see.
    #[test]
    fn only_open_channels_render_into_the_prop() {
        let mut channel = bare_channel("home", "O", "Home");
        let mentions = vec!["town-square".to_owned(), "secret-plans".to_owned()];
        let mentioned = vec![
            bare_channel("town-square", "O", "Town Square"),
            bare_channel("secret-plans", "P", "Secret Plans"),
        ];

        apply_channel_mentions_prop(&mut channel, &mentions, &mentioned);

        let props = channel.props.as_ref().expect("the prop was written");
        assert_eq!(
            props.get("channel_mentions"),
            Some(&serde_json::json!({
                "town-square": { "display_name": "Town Square" }
            })),
            "one open channel in, one entry out, keyed by Name with only display_name inside"
        );
    }

    /// The `else` branch: mentions that all miss do not merely skip the write — they delete a
    /// prop that is already there, because the header may have changed since it was computed.
    #[test]
    fn unresolved_mentions_delete_a_stale_prop() {
        let mut channel = bare_channel("home", "O", "Home");
        channel.add_prop("channel_mentions", serde_json::json!({"gone": {}}));
        channel.add_prop("other", serde_json::json!("survives"));

        apply_channel_mentions_prop(&mut channel, &["missing".to_owned()], &[]);

        let props = channel.props.as_ref().expect("the map itself survives");
        assert!(
            !props.contains_key("channel_mentions"),
            "the stale prop is deleted, not left behind"
        );
        assert_eq!(
            props.get("other"),
            Some(&serde_json::json!("survives")),
            "only the one key is deleted"
        );
    }

    /// `AddProp` replaces the value wholesale — a fresh computation is never merged into a stale
    /// one.
    #[test]
    fn a_resolved_mention_replaces_the_prop_not_merges_it() {
        let mut channel = bare_channel("home", "O", "Home");
        channel.add_prop(
            "channel_mentions",
            serde_json::json!({"stale": {"display_name": "Stale"}}),
        );

        apply_channel_mentions_prop(
            &mut channel,
            &["fresh".to_owned()],
            &[bare_channel("fresh", "O", "Fresh")],
        );

        assert_eq!(
            channel
                .props
                .as_ref()
                .and_then(|p| p.get("channel_mentions")),
            Some(&serde_json::json!({"fresh": {"display_name": "Fresh"}})),
            "the stale key is gone because the whole value was replaced"
        );
    }

    /// Go's `len > 0` guard sits above the delete too: a header with no `~` leaves even a stale
    /// prop untouched. The store is unreachable, so this also proves no query is issued.
    #[tokio::test]
    async fn a_header_without_mentions_touches_nothing_and_queries_nothing() {
        let app = unreachable_app();
        let mut channel = bare_channel("home", "O", "Home");
        channel.header = "no mentions here".to_owned();
        channel.add_prop("channel_mentions", serde_json::json!({"stale": {}}));
        let before = channel.clone();

        app.fill_in_channel_props(&mut channel)
            .await
            .expect("no mentions means no store call, so the dead pool is never touched");
        assert_eq!(channel, before, "the stale prop survives an emptied header");
    }

    /// A header **with** a mention against a broken store is Go's 500 with Go's error id — the
    /// one from `GetChannelsByNames`, not a new one invented for the props pass.
    #[tokio::test]
    async fn a_broken_mention_lookup_is_gos_500() {
        let app = unreachable_app();
        let mut channel = bare_channel("home", "O", "Home");
        channel.header = "see ~town-square".to_owned();

        let err = app
            .fill_in_channel_props(&mut channel)
            .await
            .expect_err("the store is unreachable and a mention forces a query");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.channel.get_by_name.existing.app_error");
    }
}
