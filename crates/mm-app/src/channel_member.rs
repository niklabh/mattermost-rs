//! Port of the channel-membership **write** paths of `server/channels/app/channel.go`.
//!
//! Six api4 routes sit on this file:
//!
//! | route | app entry point |
//! |---|---|
//! | `POST   /channels/{id}/members` | [`App::add_channel_member`] |
//! | `PUT    /channels/{id}/members` | [`App::set_channel_members`] |
//! | `DELETE /channels/{id}/members/{user}` | [`App::remove_user_from_channel`] |
//! | `PUT    /channels/{id}/members/{user}/notify_props` | [`App::update_channel_member_notify_props`] |
//! | `PUT    /channels/{id}/members/{user}/roles` | [`App::update_channel_member_roles`] |
//! | `PUT    /channels/{id}/members/{user}/schemeRoles` | [`App::update_channel_member_scheme_roles`] |
//!
//! # The join and leave system posts are **not** written here
//!
//! `AddChannelMember` ends in `postJoinChannelMessage`/`PostAddToChannelMessage` and
//! `RemoveUserFromChannel` in `postLeaveChannelMessage`/`postRemoveFromChannelMessage`. Those are
//! writes to `Posts`, which this port does not have yet — see **D-231**. Every one of them is
//! outside the response body, so no test of these six routes' HTTP answers can see the gap; what a
//! client sees is a channel whose membership changed with no system message in the timeline.
//!
//! Two of the four are *not* fire-and-forget in Go and their absence therefore also removes an
//! error branch: a self-add whose join post fails is a failed `POST /members` in Go
//! (`return nil, err`), and a self-removal whose leave post fails is a failed `DELETE`. Both are
//! unreachable in practice — the post write would have to fail on a channel the caller just
//! joined — but they are branches this file does not have.
//!
//! # What is forwarded rather than answered
//!
//! Every branch that needs machinery this server does not have is refused as
//! [`MemberWrite::Forward`] and the handler hands the whole request to Go, so the answer is Go's
//! own. The reasons are enumerated on each function; the recurring ones are group-constrained
//! channels (the group store's `FilterNonGroupChannelMembers`), attribute-based access control,
//! shared channels, guest sessions, `post_root_id` (a `ThreadMemberships` write) and a channel
//! carrying a `default_category_name` (a `SidebarChannels` write).

use mm_model::channel::Channel;
use mm_model::channel_member::{ChannelMember, get_default_channel_notify_props};
use mm_model::role::{
    CHANNEL_ADMIN_ROLE_ID, CHANNEL_GUEST_ROLE_ID, CHANNEL_USER_ROLE_ID, is_built_in_role,
    is_channel_scoped_built_in_role,
};
use mm_model::user::{
    DESKTOP_NOTIFY_PROP, DESKTOP_SOUND_NOTIFY_PROP, DESKTOP_THREADS_NOTIFY_PROP, EMAIL_NOTIFY_PROP,
    MARK_UNREAD_NOTIFY_PROP, PUSH_NOTIFY_PROP, PUSH_THREADS_NOTIFY_PROP, User,
};
use mm_model::utils::{AppError, AppResult, StringMap, get_millis};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_CHANNEL_MEMBER_UPDATED, WEBSOCKET_EVENT_USER_ADDED,
    WEBSOCKET_EVENT_USER_REMOVED, WebSocketEvent,
};
use mm_store::channel_member_history_store::ChannelMemberHistoryStore;
use mm_store::channel_store::ChannelStore;
use mm_store::group_store::GroupStore;
use mm_store::thread_store::ThreadStore;

use crate::App;

/// The `channel_auto_follow_threads` notify prop, whose constant Go keeps on `model` rather than
/// with the other notify props.
const CHANNEL_AUTO_FOLLOW_THREADS: &str = "channel_auto_follow_threads";

/// `desktop_notification_sound` is a **string literal** in Go's filter list
/// (app/channel.go:1543) where its nine neighbours are named constants. Copied as a literal so the
/// two lists read the same way.
const DESKTOP_NOTIFICATION_SOUND: &str = "desktop_notification_sound";

/// The Go names of the ten keys `UpdateChannelMemberNotifyProps` will accept, in Go's order.
///
/// **This is a filter, not a validator.** Nothing on the update path calls
/// `IsChannelMemberNotifyPropsValid` — see [`App::update_channel_member_notify_props`].
const UPDATABLE_NOTIFY_PROPS: [&str; 10] = [
    MARK_UNREAD_NOTIFY_PROP,
    DESKTOP_NOTIFY_PROP,
    DESKTOP_SOUND_NOTIFY_PROP,
    DESKTOP_NOTIFICATION_SOUND,
    DESKTOP_THREADS_NOTIFY_PROP,
    EMAIL_NOTIFY_PROP,
    PUSH_NOTIFY_PROP,
    PUSH_THREADS_NOTIFY_PROP,
    mm_model::channel_member::IGNORE_CHANNEL_MENTIONS_NOTIFY_PROP,
    CHANNEL_AUTO_FOLLOW_THREADS,
];

/// Go's `model.DefaultChannelName` (model/channel.go) — the channel nobody may be removed from.
const DEFAULT_CHANNEL_NAME: &str = "town-square";

/// A membership write that either happened here or must be handed to Go whole.
///
/// The same shape as [`crate::draft::DraftWrite`]: the app layer decides, the handler forwards.
/// `Forward` carries the reason so a log line names the Go branch that could not be reproduced
/// rather than merely reporting a miss.
#[derive(Debug)]
pub enum MemberWrite<T> {
    Done(T),
    Forward(&'static str),
}

/// Port of `app.ChannelMemberOpts` (app/channel.go:1996).
#[derive(Debug, Default, Clone)]
pub struct ChannelMemberOpts {
    pub user_requestor_id: String,
    pub post_root_id: String,
    pub skip_team_member_integrity_check: bool,
}

impl App {
    /// Port of `app.App.GetSchemeRolesForChannel` (app/channel.go:1119).
    ///
    /// The channel's own scheme wins; otherwise the **team's** scheme, read for its *channel*
    /// role defaults; otherwise the three constants. Note that both scheme lookups go through
    /// [`App::get_scheme`], which is gated on the advanced-permissions phase-2 migration — so on a
    /// server where that migration has not run, a channel with a scheme answers **501** here
    /// rather than falling back to the constants.
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id))]
    pub async fn get_scheme_roles_for_channel(
        &self,
        channel_id: &str,
    ) -> AppResult<(String, String, String)> {
        let channel = self.get_channel(channel_id).await?;

        if let Some(scheme_id) = channel.scheme_id.as_deref()
            && !scheme_id.is_empty()
        {
            let scheme = self.get_scheme(scheme_id).await?;
            return Ok((
                scheme.default_channel_guest_role,
                scheme.default_channel_user_role,
                scheme.default_channel_admin_role,
            ));
        }

        self.get_team_scheme_channel_roles(&channel.team_id).await
    }

    /// Port of `app.App.GetTeamSchemeChannelRoles` (app/channel.go:1143).
    ///
    /// The three columns read from a team scheme are `DefaultChannel*Role`, **not**
    /// `DefaultTeam*Role`: this answers "what does a *channel member* of a channel in this team
    /// fall back to". Substituting the team roles would hand channel members a team-scoped role
    /// name whose permission set is a different one entirely.
    #[tracing::instrument(skip(self), fields(team_id = %team_id))]
    pub async fn get_team_scheme_channel_roles(
        &self,
        team_id: &str,
    ) -> AppResult<(String, String, String)> {
        let team = self.get_team(team_id).await?;

        if let Some(scheme_id) = team.scheme_id.as_deref()
            && !scheme_id.is_empty()
        {
            let scheme = self.get_scheme(scheme_id).await?;
            return Ok((
                scheme.default_channel_guest_role,
                scheme.default_channel_user_role,
                scheme.default_channel_admin_role,
            ));
        }

        Ok((
            CHANNEL_GUEST_ROLE_ID.to_owned(),
            CHANNEL_USER_ROLE_ID.to_owned(),
            CHANNEL_ADMIN_ROLE_ID.to_owned(),
        ))
    }

    /// Port of `app.App.UpdateChannelMemberRoles` (app/channel.go:1404) and the
    /// `updateChannelMemberRolesInternal` (:1416) it delegates to with `allowSchemeUserUnset =
    /// false`.
    ///
    /// # Every scheme flag is cleared first, then rebuilt from the submitted string
    ///
    /// This is a **set**, not a patch: `SchemeGuest`, `SchemeUser` and `SchemeAdmin` are all set to
    /// `false` before the loop, so a caller who sends `"channel_admin"` alone is asking to be an
    /// admin who is not a user — which the `unset_user_scheme` check below then refuses. Sending
    /// the roles the member already has is how a client keeps them.
    ///
    /// # Five refusals, and their order is on the wire
    ///
    /// 1. **A role that does not exist** — `GetRoleByName`'s error, forced to **400**. Note the
    ///    status is overwritten: `GetRoleByName` answers 404 for a missing role and this rewrites
    ///    it, so a typo'd role name is a 400 with a *404's* error id
    ///    (`app.role.get_by_name.app_error`).
    /// 2. **A non-scheme-managed built-in role that is not channel-scoped** — e.g. `system_admin`
    ///    — is `update_channel_member_roles.scheme_role.app_error`. A *custom* role (not built-in)
    ///    is accepted as an explicit role.
    /// 3. **A scheme-managed role that is not one of this channel's three** is the same id.
    /// 4. **Guest and user together**, then **guest and admin together**.
    /// 5. **Changing the guest flag either way** — `prevSchemeGuestValue != member.SchemeGuest` —
    ///    which is what stops this route being used to promote a guest to a member.
    ///
    /// The guest-flag comparison is against the value the member *had*, so it fires both for
    /// promoting a guest and for demoting a member to one.
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id, user_id = %user_id, roles = %new_roles))]
    pub async fn update_channel_member_roles(
        &self,
        channel_id: &str,
        user_id: &str,
        new_roles: &str,
    ) -> AppResult<ChannelMember> {
        let mut member = self.get_channel_member(channel_id, user_id).await?;

        let (scheme_guest_role, scheme_user_role, scheme_admin_role) =
            self.get_scheme_roles_for_channel(channel_id).await?;

        let prev_scheme_guest = member.scheme_guest;

        let mut new_explicit_roles: Vec<&str> = Vec::new();
        member.scheme_guest = false;
        member.scheme_user = false;
        member.scheme_admin = false;

        for role_name in new_roles.split_whitespace() {
            let role = self.get_role_by_name(role_name).await.map_err(|mut err| {
                // `err.StatusCode = http.StatusBadRequest` — the id stays whatever
                // `GetRoleByName` produced.
                err.status_code = 400;
                err
            })?;

            if !role.scheme_managed {
                if is_built_in_role(role_name) && !is_channel_scoped_built_in_role(role_name) {
                    return Err(scheme_role_error(role_name));
                }
                new_explicit_roles.push(role_name);
            } else if role_name == scheme_admin_role {
                member.scheme_admin = true;
            } else if role_name == scheme_user_role {
                member.scheme_user = true;
            } else if role_name == scheme_guest_role {
                member.scheme_guest = true;
            } else {
                // Scheme-managed, but not part of *this* channel's scheme.
                return Err(scheme_role_error(role_name));
            }
        }

        if member.scheme_user && member.scheme_guest {
            return Err(roles_error("guest_and_user"));
        }
        if member.scheme_guest && member.scheme_admin {
            return Err(roles_error("guest_and_admin"));
        }
        if prev_scheme_guest != member.scheme_guest {
            return Err(roles_error("changing_guest_role"));
        }
        if !member.scheme_guest && !member.scheme_user {
            return Err(roles_error("unset_user_scheme"));
        }

        member.explicit_roles = new_explicit_roles.join(" ");

        self.update_channel_member(member).await
    }

    /// Port of `app.App.UpdateChannelMemberSchemeRoles` (app/channel.go:1489).
    ///
    /// # Three of the eight combinations are reachable
    ///
    /// The booleans arrive from the request body, and `isSchemeGuest = true` is refused outright
    /// while `isSchemeUser = false` is refused too — so the only accepted bodies are
    /// `scheme_user: true` with `scheme_admin` either way. Everything else is a 400, in this
    /// order:
    ///
    /// 1. **The member is currently a guest** → `update_channel_member_roles.guest.app_error`.
    ///    Checked against the *stored* member, before the body is looked at.
    /// 2. **`scheme_guest: true`** → `…user_and_guest.app_error`.
    /// 3. **`scheme_user: false`** → `…unset_user_scheme.app_error`.
    ///
    /// Note all three ids live in the `update_channel_member_roles.*` family even though this is a
    /// different route, and 2 and 3 are ids the `/roles` route never produces for the same input.
    ///
    /// # The phase-2 migration gate is inverted here
    ///
    /// `if err = a.IsPhase2MigrationCompleted(); err != nil` — the roles are stripped **when the
    /// migration has *not* finished**, and the error is discarded. So on a migrated server (every
    /// server this port runs against) `explicit_roles` is left alone; on an unmigrated one the
    /// three built-in channel role ids are removed from it. Reading that condition the other way
    /// round would silently drop a member's explicit roles on every scheme-role update.
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id, user_id = %user_id))]
    pub async fn update_channel_member_scheme_roles(
        &self,
        channel_id: &str,
        user_id: &str,
        is_scheme_guest: bool,
        is_scheme_user: bool,
        is_scheme_admin: bool,
    ) -> AppResult<ChannelMember> {
        let mut member = self.get_channel_member(channel_id, user_id).await?;

        if member.scheme_guest {
            return Err(scheme_roles_error("guest"));
        }
        if is_scheme_guest {
            return Err(scheme_roles_error("user_and_guest"));
        }
        if !is_scheme_user {
            return Err(scheme_roles_error("unset_user_scheme"));
        }

        member.scheme_admin = is_scheme_admin;
        member.scheme_user = is_scheme_user;
        member.scheme_guest = is_scheme_guest;

        if self.is_phase2_migration_completed().await.is_err() {
            member.explicit_roles = remove_roles(
                &[
                    CHANNEL_GUEST_ROLE_ID,
                    CHANNEL_USER_ROLE_ID,
                    CHANNEL_ADMIN_ROLE_ID,
                ],
                &member.explicit_roles,
            );
        }

        self.update_channel_member(member).await
    }

    /// Port of `app.App.UpdateChannelMemberNotifyProps` (app/channel.go:1519).
    ///
    /// # It filters, it does **not** validate
    ///
    /// Ten known keys are copied out of the submitted map and everything else is dropped; nothing
    /// on this path calls `IsChannelMemberNotifyPropsValid`, and neither does the store's
    /// `UpdateMemberNotifyProps`. So `{"desktop": "banana"}` is a **200** that stores `banana`,
    /// while the same value reaching `ChannelMember.IsValid` through the *add* path would be a
    /// 400. Measured against the running Go server, not inferred.
    ///
    /// # The write is a merge
    ///
    /// A key the caller omitted keeps its stored value — see
    /// [`mm_store::channel_store::update_member_notify_props`]. That is why this route can be used
    /// to set one prop without reading the others first, and why replacing the column instead
    /// would clear a member's mute.
    ///
    /// # An empty filtered map is still a write
    ///
    /// A body naming none of the ten keys reaches the store with `{}`, which merges to a no-op —
    /// but `LastUpdateAt` still moves and the `channel_member_updated` event is still published.
    #[tracing::instrument(skip(self, data), fields(channel_id = %channel_id, user_id = %user_id, submitted = data.len()))]
    pub async fn update_channel_member_notify_props(
        &self,
        data: &StringMap,
        channel_id: &str,
        user_id: &str,
    ) -> AppResult<ChannelMember> {
        let mut filtered = StringMap::new();
        for key in UPDATABLE_NOTIFY_PROPS {
            if let Some(value) = data.get(key) {
                filtered.insert(key.to_owned(), value.clone());
            }
        }

        let member = self
            .store()
            .channel()
            .update_member_notify_props(channel_id, user_id, &filtered)
            .await
            .map_err(|err| match err {
                mm_store::StoreError::Invalid { app_error, .. } => app_error,
                mm_store::StoreError::InvalidInput { .. } => AppError::boxed(
                    "updateMemberNotifyProps",
                    "app.channel.update_member.notify_props_limit_exceeded.app_error",
                    None,
                    String::new(),
                    400,
                ),
                mm_store::StoreError::NotFound { .. } => AppError::boxed(
                    "updateMemberNotifyProps",
                    "app.channel.get_member.missing.app_error",
                    None,
                    String::new(),
                    404,
                ),
                other => {
                    tracing::error!(error = %other, "notify-props update failed");
                    AppError::boxed(
                        "updateMemberNotifyProps",
                        "app.channel.update_member.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        // Go's error id for a marshal failure here is `api.marshal_error` at 500; ours cannot
        // fail, because `send_update_channel_member_event` logs rather than returns.
        self.send_update_channel_member_event(&member).await;

        Ok(member)
    }

    /// Port of `app.App.updateChannelMember` (app/channel.go:1663) — the store write plus the
    /// `channel_member_updated` event that `/roles` and `/schemeRoles` both end in.
    ///
    /// The event is addressed to the **member's user id and nothing else** — no channel, no team —
    /// so it reaches that one user's sessions and no other member of the channel learns that
    /// somebody's roles changed. Adding a `channel_id` here would broadcast a member's roles to
    /// everyone in the channel.
    ///
    /// The payload is the member **JSON-encoded into a string** under `channelMember`, not a
    /// nested object. A client parses it with a second `JSON.parse`, so an object there would
    /// break every existing client.
    #[tracing::instrument(skip(self, member), fields(channel_id = %member.channel_id, user_id = %member.user_id))]
    pub async fn update_channel_member(&self, member: ChannelMember) -> AppResult<ChannelMember> {
        let updated =
            self.store()
                .channel()
                .update_member(member)
                .await
                .map_err(|err| match err {
                    mm_store::StoreError::Invalid { app_error, .. } => app_error,
                    mm_store::StoreError::NotFound { .. } => AppError::boxed(
                        "updateChannelMember",
                        "app.channel.get_member.missing.app_error",
                        None,
                        String::new(),
                        404,
                    ),
                    other => {
                        tracing::error!(error = %other, "channel member update failed");
                        AppError::boxed(
                            "updateChannelMember",
                            "app.channel.get_member.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                })?;

        self.send_update_channel_member_event(&updated).await;

        Ok(updated)
    }

    /// Port of `app.App.sendUpdateChannelMemberEvent` (app/channel.go:1651).
    async fn send_update_channel_member_event(&self, member: &ChannelMember) {
        let mut event = WebSocketEvent::new(
            WEBSOCKET_EVENT_CHANNEL_MEMBER_UPDATED,
            "",
            "",
            &member.user_id,
            None,
            "",
        );
        match serde_json::to_string(member) {
            Ok(json) => event.add("channelMember", serde_json::Value::String(json)),
            Err(err) => {
                // Go returns the marshal error and the caller turns it into a 500; a
                // `ChannelMember` cannot fail to encode, so this is a log line rather than a
                // fabricated error branch.
                tracing::warn!(error = %err, "failed to encode a ChannelMember for the socket");
                return;
            }
        }
        self.publish(event).await;
    }

    /// Port of `app.App.AddChannelMember` (app/channel.go:2004).
    ///
    /// # An existing member is returned untouched, before anything else
    ///
    /// The first statement is a `GetMember`, and a hit returns that member with **no** write, no
    /// event and no system post. So `POST /members` is idempotent for a member who is already in
    /// the channel — which is what lets the handler's public-channel self-add branch answer 201
    /// for a caller holding no permission at all.
    ///
    /// # A deactivated user is a 403, not a 400
    ///
    /// `user.DeleteAt > 0` → `app.channel.add_member.deleted_user.app_error` at **403**, which is
    /// the only 403 this function raises and reads like a permission error while being a state
    /// one.
    #[tracing::instrument(skip(self, channel, opts), fields(channel_id = %channel.id, user_id = %user_id))]
    pub async fn add_channel_member(
        &self,
        user_id: &str,
        channel: &Channel,
        opts: &ChannelMemberOpts,
    ) -> Result<MemberWrite<ChannelMember>, Box<AppError>> {
        match self
            .store()
            .channel()
            .get_member(&channel.id, user_id)
            .await
        {
            Ok(member) => return Ok(MemberWrite::Done(member)),
            Err(err) if err.is_not_found() => {}
            Err(err) => {
                tracing::error!(error = %err, "channel member lookup failed");
                return Err(AppError::boxed(
                    "AddChannelMember",
                    "app.channel.get_member.app_error",
                    None,
                    String::new(),
                    500,
                ));
            }
        }

        let user = self.get_user(user_id).await?;

        if user.delete_at > 0 {
            return Err(AppError::boxed(
                "AddChannelMember",
                "app.channel.add_member.deleted_user.app_error",
                None,
                String::new(),
                403,
            ));
        }

        // Go loads the requestor only to hand it to the plugin hook and the add-to-channel post;
        // the lookup itself can fail the request, so it is not optional.
        if !opts.user_requestor_id.is_empty() {
            self.get_user(&opts.user_requestor_id).await?;
        }

        let member = match self
            .add_user_to_channel(&user, channel, opts.skip_team_member_integrity_check)
            .await?
        {
            MemberWrite::Done(member) => member,
            MemberWrite::Forward(why) => return Ok(MemberWrite::Forward(why)),
        };

        // `UserHasJoinedChannel` is a plugin hook run on `a.Srv().Go` — with no plugin
        // environment it is a no-op, and it cannot fail the request either way.
        //
        // The join/add system post is D-231; see the module docs.

        Ok(MemberWrite::Done(member))
    }

    /// Port of `app.App.AddUserToChannel` (app/channel.go:1949) and the `addUserToChannel`
    /// (:1847) it wraps.
    ///
    /// # The team-membership check is a 404 and it comes first
    ///
    /// Unless the caller skips it, a user who is not on the channel's team is
    /// `app.team.get_member.missing.app_error` at **404**, and one whose team membership is
    /// *deleted* is `api.channel.add_user.to.channel.failed.deleted.app_error` at 400. Two
    /// different statuses for what a client would call the same problem.
    ///
    /// # A new member's three flags
    ///
    /// `SchemeGuest = user.IsGuest()`, `SchemeUser = !user.IsGuest()`, and `SchemeAdmin` from
    /// `UserIsInAdminRoleGroup` — asked only for a non-guest. `NotifyProps` is the six-key default
    /// map, and it must be present: `ChannelMember::is_valid` rejects a member with no `desktop`
    /// key, so an empty map here would make every add a 400 from inside the store.
    ///
    /// # Two `user_added` events, deliberately
    ///
    /// One addressed to the **channel** (with the added user in `omit_users`, so they do not get
    /// it twice) and one addressed to the **added user**. Go's comment explains why: a cluster
    /// node that has not seen the new membership yet would filter the channel-addressed event out
    /// for that user. Both carry `user_id` and `team_id` in `data`. Publishing only the first
    /// leaves the joining client waiting for an event it never gets.
    ///
    /// # What is forwarded
    ///
    /// - a **group-constrained** channel (`FilterNonGroupChannelMembers`),
    /// - a **private** channel under attribute-based access control,
    /// - a **shared** channel (`NotifyMembershipChanged`),
    /// - a channel with a **`default_category_name`** (`addChannelToDefaultCategory` writes
    ///   `SidebarChannels`).
    #[tracing::instrument(skip(self, user, channel), fields(channel_id = %channel.id, user_id = %user.id))]
    pub async fn add_user_to_channel(
        &self,
        user: &User,
        channel: &Channel,
        skip_team_member_integrity_check: bool,
    ) -> Result<MemberWrite<ChannelMember>, Box<AppError>> {
        if !skip_team_member_integrity_check {
            let team_member = self.get_team_member(&channel.team_id, &user.id).await;
            match team_member {
                Ok(team_member) => {
                    if team_member.delete_at > 0 {
                        return Err(AppError::boxed(
                            "AddUserToChannel",
                            "api.channel.add_user.to.channel.failed.deleted.app_error",
                            None,
                            String::new(),
                            400,
                        ));
                    }
                }
                Err(err) if err.status_code == 404 => {
                    return Err(AppError::boxed(
                        "AddUserToChannel",
                        "app.team.get_member.missing.app_error",
                        None,
                        String::new(),
                        404,
                    ));
                }
                Err(err) => return Err(err),
            }
        }

        match self.add_user_to_channel_row(user, channel).await? {
            MemberWrite::Forward(why) => return Ok(MemberWrite::Forward(why)),
            MemberWrite::Done(new_member) => {
                if channel.is_shared() {
                    return Ok(MemberWrite::Forward(
                        "a shared channel's membership change has to reach the remote cluster",
                    ));
                }
                if !channel.default_category_name.is_empty() {
                    return Ok(MemberWrite::Forward(
                        "addChannelToDefaultCategory writes SidebarChannels",
                    ));
                }

                let mut channel_event = WebSocketEvent::new(
                    WEBSOCKET_EVENT_USER_ADDED,
                    "",
                    &channel.id,
                    "",
                    Some(std::collections::BTreeMap::from([(user.id.clone(), true)])),
                    "",
                );
                channel_event.add("user_id", serde_json::Value::String(user.id.clone()));
                channel_event.add(
                    "team_id",
                    serde_json::Value::String(channel.team_id.clone()),
                );
                self.publish(channel_event).await;

                let mut user_event = WebSocketEvent::new(
                    WEBSOCKET_EVENT_USER_ADDED,
                    "",
                    &channel.id,
                    &user.id,
                    None,
                    "",
                );
                user_event.add("user_id", serde_json::Value::String(user.id.clone()));
                user_event.add(
                    "team_id",
                    serde_json::Value::String(channel.team_id.clone()),
                );
                self.publish(user_event).await;

                Ok(MemberWrite::Done(new_member))
            }
        }
    }

    /// The row-writing half of `addUserToChannel` (app/channel.go:1847): the type check, the
    /// already-a-member shortcut, the new member's flags, the insert and the join history row.
    ///
    /// Split out from [`App::add_user_to_channel`] only because Go's outer function is what
    /// publishes the events, and keeping the two apart makes the ordering visible: the history row
    /// is written **before** any event goes out.
    async fn add_user_to_channel_row(
        &self,
        user: &User,
        channel: &Channel,
    ) -> Result<MemberWrite<ChannelMember>, Box<AppError>> {
        // `!= 'O' && != 'P' && !IsSpace()`. The handler has already refused DM/GM with its own
        // id, and a space channel with a third, so this is Go's belt-and-braces check reproduced
        // rather than a reachable branch on these routes.
        if channel.channel_type != mm_model::channel::CHANNEL_TYPE_OPEN
            && channel.channel_type != mm_model::channel::CHANNEL_TYPE_PRIVATE
            && !channel.is_space()
        {
            return Err(AppError::boxed(
                "AddUserToChannel",
                "api.channel.add_user_to_channel.type.app_error",
                None,
                String::new(),
                400,
            ));
        }

        match self
            .store()
            .channel()
            .get_member(&channel.id, &user.id)
            .await
        {
            Ok(member) => return Ok(MemberWrite::Done(member)),
            Err(err) if err.is_not_found() => {}
            Err(err) => {
                tracing::error!(error = %err, "channel member lookup failed");
                return Err(AppError::boxed(
                    "AddUserToChannel",
                    "app.channel.get_member.app_error",
                    None,
                    String::new(),
                    500,
                ));
            }
        }

        if channel.is_group_constrained() {
            return Ok(MemberWrite::Forward(
                "FilterNonGroupChannelMembers needs the group syncable store",
            ));
        }

        if channel.channel_type == mm_model::channel::CHANNEL_TYPE_PRIVATE
            && channel.has_membership_policy_action()
        {
            return Ok(MemberWrite::Forward(
                "an access-controlled channel needs the ABAC policy evaluator",
            ));
        }

        let scheme_admin = if user.is_guest() {
            false
        } else {
            !self
                .store()
                .group()
                .admin_role_groups_for_channel_member(&user.id, &channel.id)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "admin-role group lookup failed");
                    AppError::boxed(
                        "UserIsInAdminRoleGroup",
                        "app.select_error",
                        None,
                        String::new(),
                        500,
                    )
                })?
                .is_empty()
        };

        let new_member = ChannelMember {
            channel_id: channel.id.clone(),
            user_id: user.id.clone(),
            notify_props: Some(get_default_channel_notify_props()),
            scheme_guest: user.is_guest(),
            scheme_user: !user.is_guest(),
            scheme_admin,
            ..ChannelMember::default()
        };

        // `runGuardedChannelMemberWillBeAdded` is a plugin hook; with no plugin environment it is
        // the identity, so the member reaches the store unchanged.
        let saved = self
            .store()
            .channel()
            .save_member(new_member)
            .await
            // `IsValid` runs inside the store and Go's `AddUserToChannel` does **not** unwrap
            // it — every store failure, validation included, becomes one 500 with
            // `api.channel.add_user.to.channel.failed.app_error`. So a malformed member is a 500
            // here and a 400 on the update paths, for the same `model.channel_member.is_valid`
            // failure.
            .map_err(|err| {
                tracing::error!(error = %err, "channel member save failed");
                AppError::boxed(
                    "AddUserToChannel",
                    "api.channel.add_user.to.channel.failed.app_error",
                    None,
                    format!(
                        "failed to add member: {err}, user_id: {}, channel_id: {}",
                        user.id, channel.id
                    ),
                    500,
                )
            })?;

        self.store()
            .channel_member_history()
            .log_join_event(&user.id, &channel.id, get_millis())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "join history write failed");
                AppError::boxed(
                    "AddUserToChannel",
                    "app.channel_member_history.log_join_event.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(MemberWrite::Done(saved))
    }

    /// Port of `app.App.RemoveUserFromChannel` (app/channel.go:3113) and the
    /// `removeUserFromChannel` (:2999) it wraps.
    ///
    /// # Town Square is not leavable, unless you are a guest
    ///
    /// `channel.Name == "town-square"` and the user is not a guest → `api.channel.remove.default`
    /// at 400. A guest *may* be removed from the default channel, because that is how a guest
    /// leaves a team.
    ///
    /// # Two `user_removed` events with **different** payload keys
    ///
    /// The channel-addressed one carries `user_id` and `remover_id`; the user-addressed one carries
    /// `channel_id` and `remover_id` — it has to, because its broadcast has no channel and the
    /// client has nothing else to tell it which channel it just left. Copying the first event's
    /// payload into the second is a mistake no assertion on the *number* of events would catch.
    ///
    /// # What is forwarded
    ///
    /// - a **guest** being removed, whose last channel membership evicts them from the team
    ///   (`teamService.RemoveTeamMember`, a `TeamMembers` write plus its own posts and events),
    /// - a **group-constrained** channel when somebody else is doing the removing,
    /// - a **shared** channel.
    #[tracing::instrument(skip(self, channel), fields(channel_id = %channel.id, user_id = %user_id_to_remove))]
    pub async fn remove_user_from_channel(
        &self,
        user_id_to_remove: &str,
        remover_user_id: &str,
        channel: &Channel,
    ) -> Result<MemberWrite<()>, Box<AppError>> {
        let user = self.get_user(user_id_to_remove).await.map_err(|mut err| {
            // Go's ids here are `MissingAccountError`/`app.user.get.app_error`, which is what
            // `App::get_user` already produces — but the `where` is this function's.
            err.where_ = "removeUserFromChannel".to_owned();
            err
        })?;
        let is_guest = user.is_guest();

        if channel.name == DEFAULT_CHANNEL_NAME && !is_guest {
            let params = std::collections::HashMap::from([(
                "Channel".to_owned(),
                serde_json::Value::String(DEFAULT_CHANNEL_NAME.to_owned()),
            )]);
            return Err(AppError::boxed(
                "RemoveUserFromChannel",
                "api.channel.remove.default.app_error",
                Some(params),
                String::new(),
                400,
            ));
        }

        if channel.is_group_constrained() && user_id_to_remove != remover_user_id && !user.is_bot {
            return Ok(MemberWrite::Forward(
                "FilterNonGroupChannelMembers needs the group syncable store",
            ));
        }

        if is_guest {
            return Ok(MemberWrite::Forward(
                "a guest's last channel evicts them from the team, which is a TeamMembers write",
            ));
        }

        if channel.is_shared() {
            return Ok(MemberWrite::Forward(
                "a shared channel's membership change has to reach the remote cluster",
            ));
        }

        // Go loads the member here only to hand it to the `UserHasLeftChannel` plugin hook, but
        // the load can fail the request — a non-member is a **404** from `GetChannelMember`, which
        // is what makes `DELETE …/members/{user}` idempotent-unfriendly: removing twice is a 404,
        // unlike `deleteDraft`.
        self.get_channel_member(&channel.id, user_id_to_remove)
            .await?;

        self.remove_channel_membership(user_id_to_remove, &channel.id)
            .await?;

        self.store()
            .channel_member_history()
            .log_leave_event(user_id_to_remove, &channel.id, get_millis())
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "leave history write failed");
                AppError::boxed(
                    "removeUserFromChannel",
                    "app.channel_member_history.log_leave_event.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let mut channel_event =
            WebSocketEvent::new(WEBSOCKET_EVENT_USER_REMOVED, "", &channel.id, "", None, "");
        channel_event.add(
            "user_id",
            serde_json::Value::String(user_id_to_remove.to_owned()),
        );
        channel_event.add(
            "remover_id",
            serde_json::Value::String(remover_user_id.to_owned()),
        );
        self.publish(channel_event).await;

        let mut user_event = WebSocketEvent::new(
            WEBSOCKET_EVENT_USER_REMOVED,
            "",
            "",
            user_id_to_remove,
            None,
            "",
        );
        user_event.add("channel_id", serde_json::Value::String(channel.id.clone()));
        user_event.add(
            "remover_id",
            serde_json::Value::String(remover_user_id.to_owned()),
        );
        self.publish(user_event).await;

        // The leave/remove system post is D-231; see the module docs.

        Ok(MemberWrite::Done(()))
    }

    /// Port of `app.App.removeChannelMembership` (app/channel.go:2989).
    ///
    /// The thread-membership delete is **not** optional and its failure is a 500 with
    /// `model.NoTranslation` as the id — the one place on these routes where the client is handed
    /// an untranslated detail string instead of an id.
    async fn remove_channel_membership(
        &self,
        user_id: &str,
        channel_id: &str,
    ) -> Result<(), Box<AppError>> {
        self.store()
            .channel()
            .remove_member(channel_id, user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "channel member delete failed");
                AppError::boxed(
                    "removeUserFromChannel",
                    "app.channel.remove_member.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        self.store()
            .thread()
            .delete_memberships_for_channel(user_id, channel_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "thread membership delete failed");
                AppError::boxed(
                    "removeUserFromChannel",
                    "",
                    None,
                    "failed to delete threadmemberships upon leaving channel".to_owned(),
                    500,
                )
            })?;

        Ok(())
    }
}

impl App {
    /// Port of `app.App.GetChannelOfType` (app/channel.go:2260).
    ///
    /// The same two-branch error shape as [`App::get_channel`] — `app.channel.get.existing` at 404,
    /// `app.channel.get.find` at 500 — and, unlike it, **no type allow-list in the query**, so this
    /// is the only way to reach a space or board channel by id. `rejectSpaceChannelByID` depends on
    /// the 404 being distinguishable, because it fails closed on anything else.
    #[tracing::instrument(skip(self), fields(channel_id = %channel_id, channel_type = %channel_type))]
    pub async fn get_channel_of_type(
        &self,
        channel_id: &str,
        channel_type: &str,
    ) -> AppResult<Channel> {
        let params = std::collections::HashMap::from([(
            "channel_id".to_owned(),
            serde_json::Value::String(channel_id.to_owned()),
        )]);
        self.store()
            .channel()
            .get_channel_of_type(channel_id, channel_type)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetChannelOfType",
                        "app.channel.get.existing.app_error",
                        Some(params),
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "typed channel lookup failed");
                    AppError::boxed(
                        "GetChannelOfType",
                        "app.channel.get.find.app_error",
                        Some(params),
                        String::new(),
                        500,
                    )
                }
            })
    }

    /// Port of `app.App.SetChannelMembers` (app/channel.go:4784) — the bulk reconcile behind
    /// `PUT /api/v4/channels/{channel_id}/members`.
    ///
    /// # Buffered here, streamed in Go
    ///
    /// Go calls `onBatch` per batch and the handler flushes each line as NDJSON. This returns the
    /// whole document instead, so the **bytes** are identical and the *timing* is not: a client
    /// watching for progress sees nothing until the end. The batch delay is still honoured, so the
    /// request takes as long as Go's; only the flushes are missing. That is the one deliberate
    /// divergence on this route.
    ///
    /// # Four phases, in this order, and the order is the contract
    ///
    /// Removals, then additions, then promotions, then demotions. Removing first is what lets a
    /// caller swap the whole membership of a channel that is at its member cap.
    ///
    /// # The empty-private guard fires before anything is computed
    ///
    /// A private channel may not be emptied — `app.channel.set_members.empty_private.app_error` at
    /// 400 — "they become orphaned since non-admins can't rejoin". Note it tests the **submitted**
    /// list, not the resulting membership, so `{"members": []}` on a private channel is refused
    /// while `{"members": ["someone-not-on-the-team"]}` is accepted and produces one error line and
    /// an empty channel.
    ///
    /// # A no-op still emits one line
    ///
    /// Nothing to add, nothing to remove and `channel_admins` absent ⇒ `onBatch(&{})` exactly once,
    /// which is `{"added":[],"removed":[]}`. Returning no lines at all would leave a client waiting.
    ///
    /// # Go's diff order is a map iteration and therefore random
    ///
    /// `for id := range desiredSet` — so which ids land in which batch, and their order within a
    /// line, is not stable across runs on the Go side. Anything asserting on these lines has to
    /// sort. This port iterates the *submitted* order, which is one of the orders Go can produce.
    #[allow(clippy::too_many_arguments)] // Go's signature, minus the callback.
    #[tracing::instrument(skip(self, channel, desired, admin_set), fields(channel_id = %channel.id, desired = desired.len()))]
    pub async fn set_channel_members(
        &self,
        channel: &Channel,
        desired: &[String],
        admin_set: Option<&[String]>,
        requestor_user_id: &str,
        batch_size: usize,
        batch_delay_ms: usize,
    ) -> Result<MemberWrite<String>, Box<AppError>> {
        let current = self
            .store()
            .channel()
            .get_all_channel_member_ids_by_channel_id(&channel.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "current membership lookup failed");
                AppError::boxed(
                    "SetChannelMembers",
                    "app.channel.set_members.get_current.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let to_add: Vec<String> = desired
            .iter()
            .filter(|id| !current.contains(id))
            .cloned()
            .collect();
        let to_remove: Vec<String> = current
            .iter()
            .filter(|id| !desired.contains(id))
            .cloned()
            .collect();

        if channel.channel_type == mm_model::channel::CHANNEL_TYPE_PRIVATE && desired.is_empty() {
            return Err(AppError::boxed(
                "SetChannelMembers",
                "app.channel.set_members.empty_private.app_error",
                None,
                String::new(),
                400,
            ));
        }

        if to_add.is_empty() && to_remove.is_empty() && admin_set.is_none() {
            return Ok(MemberWrite::Done(encode_batch(
                &mm_model::channel_member::SetChannelMembersResponse::default(),
            )));
        }

        // Pre-flight. Every condition that would make a *single* member operation forward is
        // resolved before the first write, because a half-applied reconcile handed to Go would be
        // applied twice. The channel-level ones are cheap; the per-user guest check is one lookup
        // per removal, which is the price of not double-applying.
        if channel.is_shared() {
            return Ok(MemberWrite::Forward(
                "a shared channel's membership change has to reach the remote cluster",
            ));
        }
        if !channel.default_category_name.is_empty() {
            return Ok(MemberWrite::Forward(
                "addChannelToDefaultCategory writes SidebarChannels",
            ));
        }
        for user_id in &to_remove {
            if let Ok(user) = self.get_user(user_id).await
                && user.is_guest()
            {
                return Ok(MemberWrite::Forward(
                    "a guest's last channel evicts them from the team, which is a TeamMembers write",
                ));
            }
        }

        // `if batchSize <= 0 { batchSize = 1 }`. The handler's bounds already rule it out, but a
        // plugin calling the app layer directly does not.
        let batch_size = batch_size.max(1);
        let delay = std::time::Duration::from_millis(batch_delay_ms as u64);
        let mut out = String::new();

        for (index, batch) in to_remove.chunks(batch_size).enumerate() {
            let mut result = mm_model::channel_member::SetChannelMembersResponse::default();
            for user_id in batch {
                match self
                    .remove_user_from_channel(user_id, requestor_user_id, channel)
                    .await
                {
                    Ok(MemberWrite::Done(())) => {
                        result
                            .removed
                            .get_or_insert_with(Vec::new)
                            .push(user_id.clone());
                    }
                    Ok(MemberWrite::Forward(why)) => {
                        // Unreachable: the pre-flight above resolved every forwarding condition.
                        tracing::error!(reason = why, "set_channel_members hit a late forward");
                        result.errors.push(unreproducible_line(user_id, why));
                    }
                    Err(err) => result.errors.push(error_line(user_id, &err)),
                }
            }
            out.push_str(&encode_batch(&result));
            if (index + 1) * batch_size < to_remove.len() && !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }

        for (index, batch) in to_add.chunks(batch_size).enumerate() {
            let mut result = mm_model::channel_member::SetChannelMembersResponse::default();
            let opts = ChannelMemberOpts {
                user_requestor_id: requestor_user_id.to_owned(),
                ..ChannelMemberOpts::default()
            };
            for user_id in batch {
                match self.add_channel_member(user_id, channel, &opts).await {
                    Ok(MemberWrite::Done(_)) => {
                        result
                            .added
                            .get_or_insert_with(Vec::new)
                            .push(user_id.clone());
                    }
                    Ok(MemberWrite::Forward(why)) => {
                        tracing::error!(reason = why, "set_channel_members hit a late forward");
                        result.errors.push(unreproducible_line(user_id, why));
                    }
                    Err(err) => result.errors.push(error_line(user_id, &err)),
                }
            }
            out.push_str(&encode_batch(&result));
            if (index + 1) * batch_size < to_add.len() && !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }

        if let Some(admins) = admin_set {
            // `GetMembers(..., Limit: 100000)` — Go's literal, not a paginated walk, so a channel
            // with more members than that silently reconciles only the first 100,000.
            let members = self
                .store()
                .channel()
                .get_members(&channel.id, 0, 100_000)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "member list for admin reconcile failed");
                    AppError::boxed(
                        "SetChannelMembers",
                        "app.channel.set_members.get_members.app_error",
                        None,
                        String::new(),
                        500,
                    )
                })?;

            let mut to_promote: Vec<String> = Vec::new();
            let mut to_demote: Vec<String> = Vec::new();
            for member in &members {
                let want_admin = admins.contains(&member.user_id);
                if want_admin && !member.scheme_admin {
                    to_promote.push(member.user_id.clone());
                } else if !want_admin && member.scheme_admin {
                    to_demote.push(member.user_id.clone());
                }
            }

            for (index, batch) in to_promote.chunks(batch_size).enumerate() {
                let mut result = mm_model::channel_member::SetChannelMembersResponse::default();
                for user_id in batch {
                    // `(false, true, true)` — guest off, user on, admin on. The three booleans are
                    // positional in Go and getting them out of order silently demotes.
                    match self
                        .update_channel_member_scheme_roles(&channel.id, user_id, false, true, true)
                        .await
                    {
                        Ok(_) => result.promoted.push(user_id.clone()),
                        Err(err) => result.errors.push(error_line(user_id, &err)),
                    }
                }
                out.push_str(&encode_batch(&result));
                if (index + 1) * batch_size < to_promote.len() && !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }

            for (index, batch) in to_demote.chunks(batch_size).enumerate() {
                let mut result = mm_model::channel_member::SetChannelMembersResponse::default();
                for user_id in batch {
                    match self
                        .update_channel_member_scheme_roles(
                            &channel.id,
                            user_id,
                            false,
                            true,
                            false,
                        )
                        .await
                    {
                        Ok(_) => result.demoted.push(user_id.clone()),
                        Err(err) => result.errors.push(error_line(user_id, &err)),
                    }
                }
                out.push_str(&encode_batch(&result));
                if (index + 1) * batch_size < to_demote.len() && !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Ok(MemberWrite::Done(out))
    }
}

/// One NDJSON line, with Go's `Added`/`Removed` nil-to-`[]` substitution applied.
///
/// The substitution happens in the **handler's** callback (api4/channel.go:2711), not in the app
/// layer, and it is why every line carries both keys as arrays while `promoted`, `demoted` and
/// `errors` are `omitempty` and simply vanish. Folding it into the app layer here keeps the bytes
/// in one place; the effect on the wire is identical.
fn encode_batch(result: &mm_model::channel_member::SetChannelMembersResponse) -> String {
    let mut normalised = result.clone();
    normalised.added.get_or_insert_with(Vec::new);
    normalised.removed.get_or_insert_with(Vec::new);
    match mm_model::utils::go_json_marshal(&normalised) {
        Ok(json) => json + "\n",
        Err(err) => {
            tracing::warn!(error = %err, "Error while writing response");
            String::new()
        }
    }
}

/// Go's `model.SetChannelMembersError{UserID, ID: appErr.Id, Error: appErr.Error()}`.
///
/// `appErr.Error()` is `where: message, detail` — the **unwiped** detail, because this string never
/// goes through `handleContextError`. So an error line can carry internal detail that the same
/// failure returned as a status code would have had blanked.
fn error_line(user_id: &str, err: &AppError) -> mm_model::channel_member::SetChannelMembersError {
    mm_model::channel_member::SetChannelMembersError {
        user_id: user_id.to_owned(),
        id: err.id.clone(),
        error: err.to_string(),
    }
}

/// The line for a member operation this port cannot reproduce, which the pre-flight in
/// [`App::set_channel_members`] is supposed to make unreachable. There is no Go counterpart — if
/// this ever appears on the wire it is a bug in the pre-flight, and it says so.
fn unreproducible_line(
    user_id: &str,
    why: &str,
) -> mm_model::channel_member::SetChannelMembersError {
    mm_model::channel_member::SetChannelMembersError {
        user_id: user_id.to_owned(),
        id: String::new(),
        error: why.to_owned(),
    }
}

/// Port of `app.removeRoles` (app/role.go:370).
fn remove_roles(roles_to_remove: &[&str], roles: &str) -> String {
    roles
        .split_whitespace()
        .filter(|role| !roles_to_remove.contains(role))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `api.channel.update_channel_member_roles.scheme_role.app_error` at 400, whose **detail carries
/// the role name** (`role_name=<name>`) and is therefore the only way a client learns which role
/// it got wrong — and only when `EnableDeveloper` is on.
fn scheme_role_error(role_name: &str) -> Box<AppError> {
    AppError::boxed(
        "UpdateChannelMemberRoles",
        "api.channel.update_channel_member_roles.scheme_role.app_error",
        None,
        format!("role_name={role_name}"),
        400,
    )
}

/// The four `UpdateChannelMemberRoles` refusals that share a shape: no params, no detail, 400.
fn roles_error(suffix: &str) -> Box<AppError> {
    AppError::boxed(
        "UpdateChannelMemberRoles",
        format!("api.channel.update_channel_member_roles.{suffix}.app_error"),
        None,
        String::new(),
        400,
    )
}

/// The same id family, raised from `UpdateChannelMemberSchemeRoles` — note the `where` differs and
/// the id family does not.
fn scheme_roles_error(suffix: &str) -> Box<AppError> {
    AppError::boxed(
        "UpdateChannelMemberSchemeRoles",
        format!("api.channel.update_channel_member_roles.{suffix}.app_error"),
        None,
        String::new(),
        400,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remove_roles_drops_only_the_named_ones_and_rejoins_with_one_space() {
        assert_eq!(
            remove_roles(
                &[
                    CHANNEL_GUEST_ROLE_ID,
                    CHANNEL_USER_ROLE_ID,
                    CHANNEL_ADMIN_ROLE_ID
                ],
                "channel_user  custom_role channel_admin",
            ),
            "custom_role"
        );
        // `strings.Fields` collapses runs of whitespace, so the output is re-spaced.
        assert_eq!(remove_roles(&["a"], "b   c"), "b c");
        assert_eq!(remove_roles(&["a"], ""), "");
        // Nothing left is the empty string, not a lone space.
        assert_eq!(remove_roles(&["a", "b"], "a b"), "");
    }

    /// The filter list is a *set of keys*, and the two easiest mistakes are dropping a key and
    /// mis-spelling one. Both are silent: the prop simply never takes effect.
    #[test]
    fn the_notify_prop_filter_holds_exactly_gos_ten_keys() {
        assert_eq!(
            UPDATABLE_NOTIFY_PROPS.to_vec(),
            vec![
                "mark_unread",
                "desktop",
                "desktop_sound",
                "desktop_notification_sound",
                "desktop_threads",
                "email",
                "push",
                "push_threads",
                "ignore_channel_mentions",
                "channel_auto_follow_threads",
            ]
        );
    }

    /// Every id in the roles family is spelled `api.channel.update_channel_member_roles.*`, even
    /// the ones raised from `UpdateChannelMemberSchemeRoles` — a reader "fixing" the second family
    /// to say `scheme_roles` would change what a client matches on.
    #[test]
    fn both_roles_error_families_share_one_id_prefix() {
        assert_eq!(
            roles_error("guest_and_user").id,
            "api.channel.update_channel_member_roles.guest_and_user.app_error"
        );
        assert_eq!(
            scheme_roles_error("user_and_guest").id,
            "api.channel.update_channel_member_roles.user_and_guest.app_error"
        );
        assert_eq!(roles_error("x").status_code, 400);
        assert_eq!(
            scheme_roles_error("x").where_,
            "UpdateChannelMemberSchemeRoles"
        );
        assert_eq!(roles_error("x").where_, "UpdateChannelMemberRoles");
    }
}
