//! Port of the team-membership **write** paths of `server/channels/app/team.go`.
//!
//! | route | app entry point |
//! |---|---|
//! | `PUT /teams/{id}/members/{user}/roles` | [`App::update_team_member_roles`] |
//! | `PUT /teams/{id}/members/{user}/schemeRoles` | [`App::update_team_member_scheme_roles`] |
//! | `DELETE /teams/{id}/members/{user}` | [`App::remove_user_from_team`] |
//!
//! # These are the team twins of `mm_app::channel_member`'s two role routes, and they differ
//!
//! Reading them side by side is the fastest way to get this file wrong. Three real differences:
//!
//! 1. **No built-in-role screen.** `updateChannelMemberRolesInternal` refuses a non-scheme-managed
//!    role that is built in but not channel-scoped; `updateTeamMemberRolesInternal`
//!    (team.go:384) has no such branch, so any non-scheme-managed role name that
//!    `GetRoleByName` resolves lands in `explicit_roles`.
//! 2. **The error ids are not all in the `api.team.*` family.** Two of them —
//!    `changing_guest_role` and `scheme_role` — say **`api.channel.`** while describing a team
//!    (team.go:432 and :467). That is Go's copy-paste and it is on the wire, so it is
//!    reproduced verbatim; "fixing" it would break a client matching on the id.
//! 3. **The websocket event is `memberrole_updated`, addressed to the user**, and it carries the
//!    member as a JSON *string* under `member` — not `channelMember`, and not a nested object.
//!
//! # `ClearSessionCacheForUser` is not reproduced
//!
//! Both Go paths call it after the write. This server has no in-process session cache to clear
//! (sessions are read from the shared `Sessions` table on each request), and the *Go* process's
//! cache is unreachable from here — the standing consequence recorded as **D-190**.

use mm_model::channel::CHANNEL_TYPE_OPEN;
use mm_model::channel::DEFAULT_CHANNEL_NAME;
use mm_model::channel_member::{ChannelMember, get_default_channel_notify_props};
use mm_model::role::{TEAM_ADMIN_ROLE_ID, TEAM_GUEST_ROLE_ID, TEAM_USER_ROLE_ID};
use mm_model::team::Team;
use mm_model::team_member::{TeamMember, TeamMemberWithError};
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_ADDED_TO_TEAM, WEBSOCKET_EVENT_LEAVE_TEAM, WEBSOCKET_EVENT_MEMBERROLE_UPDATED,
    WEBSOCKET_EVENT_USER_ADDED, WebSocketEvent,
};
use mm_store::sidebar_category_store::SidebarCategoryStore;
use mm_store::{
    ChannelMemberHistoryStore, ChannelStore, GroupStore, PreferenceStore, TeamStore, UserStore,
};

use crate::App;

impl App {
    /// Port of `app.App.GetSchemeRolesForTeam` (team.go:352).
    ///
    /// The team's scheme wins; otherwise the three `team_guest`/`team_user`/`team_admin`
    /// constants. Note `SchemeId` is a `*string` and Go tests **both** non-nil *and* non-empty,
    /// so a row with `SchemeId = ''` takes the constants rather than looking up the empty id.
    ///
    /// [`App::get_scheme`] is gated on the advanced-permissions phase-2 migration, so on a server
    /// where that migration has not run a team *with* a scheme answers **501** here instead of
    /// falling back to the constants — the same inherited behaviour as the channel twin.
    #[tracing::instrument(skip(self), fields(team_id = %team_id))]
    pub async fn get_scheme_roles_for_team(
        &self,
        team_id: &str,
    ) -> AppResult<(String, String, String)> {
        let team = self.get_team(team_id).await?;

        if let Some(scheme_id) = team.scheme_id.as_deref()
            && !scheme_id.is_empty()
        {
            let scheme = self.get_scheme(scheme_id).await?;
            return Ok((
                scheme.default_team_guest_role,
                scheme.default_team_user_role,
                scheme.default_team_admin_role,
            ));
        }

        Ok((
            TEAM_GUEST_ROLE_ID.to_owned(),
            TEAM_USER_ROLE_ID.to_owned(),
            TEAM_ADMIN_ROLE_ID.to_owned(),
        ))
    }

    /// Port of `app.App.UpdateTeamMemberRoles` (team.go:372), which is
    /// `updateTeamMemberRolesInternal(..., allowSchemeUserUnset: false)` (team.go:384). Only the
    /// bulk importer passes `true`, and nothing here does, so the unset-user gate below always
    /// runs.
    ///
    /// # Five refusals, all 400, and the order is on the wire
    ///
    /// 1. **`GetRoleByName` failed** for a submitted name — whatever id that produced, restamped
    ///    to 400. So an unknown role is *not* a fabricated "bad role" error; it is
    ///    `app.role.get_by_name.app_error` at 400.
    /// 2. **A scheme-managed role that is not one of this team's three** →
    ///    `api.channel.update_team_member_roles.scheme_role.app_error`, with
    ///    `role_name=<name>` in the details.
    /// 3. **Guest and user together**, then **guest and admin together**.
    /// 4. **The guest flag moved either way** — `prevSchemeGuestValue != member.SchemeGuest`,
    ///    compared against the value the *stored* member had, so it refuses both promoting a
    ///    guest and demoting a member into one.
    /// 5. **Neither guest nor user is set** — a member must keep a base scheme role.
    ///
    /// # The three flags are cleared before the loop
    ///
    /// `member.SchemeGuest/User/Admin = false` up front, so the submitted string is the whole
    /// truth: omitting `team_user` *removes* it rather than leaving it. That is what makes 5
    /// reachable at all, and it is why a caller cannot use this route to add one role.
    ///
    /// # There is no built-in-role screen here
    ///
    /// Unlike the channel twin. A non-scheme-managed role goes straight into `explicit_roles`.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, user_id = %user_id, roles = %new_roles))]
    pub async fn update_team_member_roles(
        &self,
        team_id: &str,
        user_id: &str,
        new_roles: &str,
    ) -> AppResult<TeamMember> {
        // Go reads the store directly here rather than going through `GetTeamMember`, but the
        // two error ids are the same pair, so `get_team_member` is the same answer.
        let mut member = self.get_team_member(team_id, user_id).await?;

        let (scheme_guest_role, scheme_user_role, scheme_admin_role) =
            self.get_scheme_roles_for_team(team_id).await?;

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
                new_explicit_roles.push(role_name);
            } else if role_name == scheme_admin_role {
                member.scheme_admin = true;
            } else if role_name == scheme_user_role {
                member.scheme_user = true;
            } else if role_name == scheme_guest_role {
                member.scheme_guest = true;
            } else {
                return Err(scheme_role_error(role_name));
            }
        }

        if member.scheme_guest && member.scheme_user {
            return Err(roles_error("api.team", "guest_and_user"));
        }
        if member.scheme_guest && member.scheme_admin {
            return Err(roles_error("api.team", "guest_and_admin"));
        }
        if prev_scheme_guest != member.scheme_guest {
            // `api.channel.` — Go's, not a typo here. See the module docs.
            return Err(roles_error("api.channel", "changing_guest_role"));
        }
        if !member.scheme_guest && !member.scheme_user {
            return Err(roles_error("api.team", "unset_user_scheme"));
        }

        member.explicit_roles = new_explicit_roles.join(" ");

        self.write_team_member(member).await
    }

    /// Port of `app.App.UpdateTeamMemberSchemeRoles` (team.go:479).
    ///
    /// # Only two bodies are accepted
    ///
    /// `scheme_user: true` with `scheme_admin` either way. Everything else is a 400, checked in
    /// this order:
    ///
    /// 1. **The stored member is a guest** → `api.team.update_team_member_roles.guest.app_error`.
    ///    Read before the body is looked at, so a guest is refused whatever was submitted.
    /// 2. **`scheme_guest: true`** → `…user_and_guest.app_error`.
    /// 3. **`scheme_user: false`** → `…unset_user_scheme.app_error`.
    ///
    /// All three ids sit in the `update_team_member_roles.*` family even though this is the
    /// `/schemeRoles` route, and 1 and 2 are ids `/roles` never produces.
    ///
    /// # The phase-2 migration gate is inverted
    ///
    /// `if err = a.IsPhase2MigrationCompleted(); err != nil` — the three built-in team role ids
    /// are stripped from `explicit_roles` **when the migration has *not* finished**, and the
    /// error is discarded. On a migrated server (every server this port runs against)
    /// `explicit_roles` is left alone. Reading the condition the other way round would drop a
    /// member's custom roles on every scheme-role update.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, user_id = %user_id))]
    pub async fn update_team_member_scheme_roles(
        &self,
        team_id: &str,
        user_id: &str,
        is_scheme_guest: bool,
        is_scheme_user: bool,
        is_scheme_admin: bool,
    ) -> AppResult<TeamMember> {
        let mut member = self.get_team_member(team_id, user_id).await?;

        if member.scheme_guest {
            return Err(roles_error("api.team", "guest"));
        }
        if is_scheme_guest {
            return Err(roles_error("api.team", "user_and_guest"));
        }
        if !is_scheme_user {
            return Err(roles_error("api.team", "unset_user_scheme"));
        }

        member.scheme_admin = is_scheme_admin;
        member.scheme_user = is_scheme_user;
        member.scheme_guest = is_scheme_guest;

        if self.is_phase2_migration_completed().await.is_err() {
            member.explicit_roles = remove_roles(
                &[TEAM_GUEST_ROLE_ID, TEAM_USER_ROLE_ID, TEAM_ADMIN_ROLE_ID],
                &member.explicit_roles,
            );
        }

        self.write_team_member(member).await
    }

    /// The tail both routes share: `Store().Team().UpdateMember` plus
    /// `sendUpdatedTeamMemberEvent` (team.go:526).
    ///
    /// Go's error mapping is `errors.As(nErr, &appErr)` first — so an `IsValid` failure travels
    /// up with its own id and 400 — and everything else is
    /// `app.team.save_member.save.app_error` at 500. There is **no** not-found branch: the store
    /// does not report one (see [`mm_store::team_store::update_member`]), so a membership that
    /// vanished between the read and the write answers 200 with the member the caller built.
    async fn write_team_member(&self, member: TeamMember) -> AppResult<TeamMember> {
        let updated =
            self.store()
                .team()
                .update_member(&member)
                .await
                .map_err(|err| match err {
                    mm_store::StoreError::Invalid { app_error, .. } => app_error,
                    other => {
                        tracing::error!(error = %other, "team member update failed");
                        AppError::boxed(
                            "UpdateTeamMemberRoles",
                            "app.team.save_member.save.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                })?;

        self.send_updated_team_member_event(&updated).await;

        Ok(updated)
    }

    /// Port of `app.App.sendUpdatedTeamMemberEvent` (team.go:526).
    ///
    /// Addressed to the **user** — no team id and no channel id on the broadcast, so it reaches
    /// that user's connections only and not the rest of the team. The payload is the member
    /// JSON-encoded into a *string* under `member`; a client parses it with a second
    /// `JSON.parse`, so a nested object there would break every existing client.
    pub(crate) async fn send_updated_team_member_event(&self, member: &TeamMember) {
        let mut event = WebSocketEvent::new(
            WEBSOCKET_EVENT_MEMBERROLE_UPDATED,
            "",
            "",
            &member.user_id,
            None,
            "",
        );
        match serde_json::to_string(member) {
            Ok(json) => event.add("member", serde_json::Value::String(json)),
            Err(err) => {
                // Go returns the marshal error and the caller turns it into a 500; a
                // `TeamMember` cannot fail to encode, so this is a log line rather than a
                // fabricated error branch.
                tracing::warn!(error = %err, "failed to encode a TeamMember for the socket");
                return;
            }
        }
        self.publish(event).await;
    }
}

/// `api.channel.update_team_member_roles.scheme_role.app_error` — the id really does say
/// `channel` (team.go:432) while naming a team role.
fn scheme_role_error(role_name: &str) -> Box<AppError> {
    AppError::boxed(
        "UpdateTeamMemberRoles",
        "api.channel.update_team_member_roles.scheme_role.app_error",
        None,
        format!("role_name={role_name}"),
        400,
    )
}

/// The four `<family>.update_team_member_roles.<suffix>.app_error` refusals, every one a 400 with
/// empty details. `family` is `api.team` for all but `changing_guest_role`.
fn roles_error(family: &str, suffix: &str) -> Box<AppError> {
    AppError::boxed(
        "UpdateTeamMemberRoles",
        format!("{family}.update_team_member_roles.{suffix}.app_error"),
        None,
        String::new(),
        400,
    )
}

/// Port of `app.removeRoles` (app/role.go) — every name in `roles` that is not in `to_remove`,
/// re-joined with single spaces. Go splits on `strings.Fields`, so a run of whitespace in the
/// input collapses.
fn remove_roles(to_remove: &[&str], roles: &str) -> String {
    roles
        .split_whitespace()
        .filter(|role| !to_remove.contains(role))
        .collect::<Vec<_>>()
        .join(" ")
}

impl App {
    /// Port of `app.App.AddTeamMember` (team.go:1152).
    ///
    /// `AddUserToTeam` with an **empty requestor id**, and then a *second*
    /// `added_to_team` publish on top of the one [`App::join_user_to_team`] already made. So a
    /// successful `POST /teams/{id}/members` puts **two identical events** on the socket, and a
    /// port that sent one would look correct in every body test and be wrong on the wire.
    ///
    /// The empty requestor also changes behaviour further down: `JoinDefaultChannels` skips its
    /// requestor lookup, and `JoinUserToTeam` resolves no actor for the plugin hook.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, user_id = %user_id))]
    pub async fn add_team_member(&self, team_id: &str, user_id: &str) -> AppResult<TeamMember> {
        let (_, member) = self.add_user_to_team(team_id, user_id, "").await?;
        self.publish_added_to_team(team_id, user_id).await;
        Ok(member)
    }

    /// Port of `app.App.AddTeamMembers` (team.go:1166).
    ///
    /// # `graceful` decides whether the first failure ends the request
    ///
    /// Without it the first error aborts and **the users already added stay added** — there is no
    /// transaction and no rollback, so a half-applied batch is Go's answer to a 400. With it every
    /// user gets an entry, successes carrying `member` and failures carrying `error`.
    ///
    /// # The per-user `added_to_team` publish is inside the success arm
    ///
    /// So a graceful batch publishes one event per *successful* user, and each of those users
    /// also got one from [`App::join_user_to_team`] — two apiece, as on the single-add route.
    ///
    /// # The requestor id is threaded through here and not on the single-add route
    ///
    /// `c.AppContext.Session().UserId`, where `AddTeamMember` passes `""`. The visible difference
    /// is in `JoinDefaultChannels`, which looks the requestor up and **fails the whole join** if
    /// that user is missing.
    #[tracing::instrument(skip(self, user_ids), fields(team_id = %team_id, asked = user_ids.len(), graceful))]
    pub async fn add_team_members(
        &self,
        team_id: &str,
        user_ids: &[String],
        user_requestor_id: &str,
        graceful: bool,
    ) -> AppResult<Vec<TeamMemberWithError>> {
        let mut out: Vec<TeamMemberWithError> = Vec::new();

        for user_id in user_ids {
            match self
                .add_user_to_team(team_id, user_id, user_requestor_id)
                .await
            {
                Ok((_, member)) => {
                    out.push(TeamMemberWithError {
                        user_id: user_id.clone(),
                        member: Some(member),
                        error: None,
                    });
                    self.publish_added_to_team(team_id, user_id).await;
                }
                Err(err) => {
                    if !graceful {
                        return Err(err);
                    }
                    out.push(TeamMemberWithError {
                        user_id: user_id.clone(),
                        member: None,
                        error: Some(err),
                    });
                }
            }
        }

        Ok(out)
    }

    /// Port of `app.App.AddUserToTeam` (team.go:537).
    ///
    /// Go reads the team and the user **concurrently** and then checks the team's result first,
    /// so a request naming both a missing team and a missing user reports the *team*. Sequential
    /// here: the two reads are independent and the observable order is the same.
    ///
    /// The four error ids are worth spelling out because two of them are surprising:
    /// `app.team.get.find.app_error` (404) / `app.team.get.finding.app_error` (500) for the team,
    /// and `app.user.missing_account.const` (404) / `app.user.get.app_error` (500) for the user.
    /// The first of those user ids is a **constant name**, not a translation key that resolves —
    /// Go's `MissingAccountError` (app/constants.go:7) is literally the string
    /// `"app.user.missing_account.const"`.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, user_id = %user_id))]
    pub async fn add_user_to_team(
        &self,
        team_id: &str,
        user_id: &str,
        user_requestor_id: &str,
    ) -> AppResult<(Team, TeamMember)> {
        let team =
            self.store()
                .team()
                .get(team_id)
                .await
                .map_err(|err| match err.is_not_found() {
                    true => AppError::boxed(
                        "AddUserToTeam",
                        "app.team.get.find.app_error",
                        None,
                        String::new(),
                        404,
                    ),
                    false => {
                        tracing::error!(error = %err, "team lookup failed");
                        AppError::boxed(
                            "AddUserToTeam",
                            "app.team.get.finding.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                })?;

        let user =
            self.store()
                .user()
                .get(user_id)
                .await
                .map_err(|err| match err.is_not_found() {
                    true => AppError::boxed(
                        "AddUserToTeam",
                        MISSING_ACCOUNT_ERROR,
                        None,
                        String::new(),
                        404,
                    ),
                    false => {
                        tracing::error!(error = %err, "user lookup failed");
                        AppError::boxed(
                            "AddUserToTeam",
                            "app.user.get.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                })?;

        let member = self
            .join_user_to_team(&team, &user, user_requestor_id)
            .await?;
        Ok((team, member))
    }

    /// Port of `app.App.JoinUserToTeam` (team.go:754) wrapped around
    /// `TeamService.JoinUserToTeam` (app/teams/teams.go:171).
    ///
    /// # An existing living membership short-circuits everything
    ///
    /// `alreadyAdded` returns the stored member with **no write, no sidebar categories, no
    /// default channels and no websocket event**. That is what makes `POST /members` idempotent,
    /// and it is why the route answers 201 for a member it did not add.
    ///
    /// # A *deleted* membership is revived, not re-inserted
    ///
    /// And only that path checks `GetActiveMemberCount` against `MaxUsersPerTeam`; the insert
    /// path checks the same limit inside the store instead. Two different queries, two different
    /// error ids — `app.team.join_user_to_team.max_accounts.app_error` for the revival and
    /// `…save_member.max_accounts.app_error` for the insert — and both are 400.
    ///
    /// # Everything after the membership write is a soft error
    ///
    /// The sidebar categories and the default channels are logged and swallowed: a join whose
    /// `town-square` add failed is still a 201 with a member who is on the team and in no
    /// channel. Go's, and the reason those two are not in any response body.
    ///
    /// # What this port does not do
    ///
    /// - **`Users.UpdateAt` is not bumped.** Go's `UpdateUpdateAt` sits between the membership
    ///   write and the sidebar categories, and `update_update_at` does not exist on this port's
    ///   `UserStore`. The consequence is on the wire: `GET /users/{id}` reports the old
    ///   `update_at` after a join this server served. Recorded as **D-242**.
    /// - **The join system post.** `ExperimentalEnableDefaultChannelLeaveJoinMessages` defaults
    ///   to **`true`** (config.go:874), so a stock Go server *does* post "user joined the team"
    ///   in `town-square`. This port writes no `Posts` rows — **D-243**.
    /// - **Plugin hooks** (`TeamMemberWillBeAdded`, `UserHasJoinedTeam`) — D-183.
    /// - **ABAC**: the private-team branch is gated on
    ///   [`App::team_membership_access_control_enabled`], a constant `false` on this deployment.
    #[tracing::instrument(skip(self, team, user), fields(team_id = %team.id, user_id = %user.id))]
    pub async fn join_user_to_team(
        &self,
        team: &Team,
        user: &User,
        user_requestor_id: &str,
    ) -> AppResult<TeamMember> {
        if !self.is_team_email_allowed(user, team) {
            return Err(join_error(
                "api.team.join_user_to_team.allowed_domains.app_error",
                String::new(),
                400,
            ));
        }

        let is_guest = user.is_guest();
        let mut candidate = TeamMember {
            team_id: team.id.clone(),
            user_id: user.id.clone(),
            scheme_guest: is_guest,
            scheme_user: !is_guest,
            create_at: get_millis(),
            ..TeamMember::default()
        };

        if !is_guest {
            let groups = self
                .store()
                .group()
                .admin_role_groups_for_team_member(&user.id, &team.id)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "admin role groups lookup failed");
                    join_error(
                        "app.team.join_user_to_team.save_member.app_error",
                        String::new(),
                        500,
                    )
                })?;
            candidate.scheme_admin = !groups.is_empty();
        }

        // The team's own contact address: whoever owns it joins as an admin. Checked **after**
        // the group lookup, so it wins over a group that said otherwise.
        if team.email == user.email {
            candidate.scheme_admin = true;
        }

        let (member, already_added) = match self.store().team().get_member(&team.id, &user.id).await
        {
            // Go's comment is "Membership appears to be missing. Lets try to add." — and its test
            // is `err != nil`, so a *database failure* also takes the insert path and is reported
            // as whatever the insert then says.
            Err(_) => (
                self.store()
                    .team()
                    .save_member(&candidate, self.config().max_users_per_team)
                    .await
                    .map_err(save_member_error)?,
                false,
            ),
            Ok(existing) if existing.delete_at == 0 => (existing, true),
            Ok(_) => {
                let count = self
                    .store()
                    .team()
                    .get_active_member_count(&team.id)
                    .await
                    .map_err(|err| {
                        tracing::error!(error = %err, "active member count failed");
                        join_error(
                            "app.team.get_active_member_count.app_error",
                            String::new(),
                            500,
                        )
                    })?;
                if count >= self.config().max_users_per_team {
                    return Err(join_error(
                        "app.team.join_user_to_team.max_accounts.app_error",
                        format!("teamId={}", team.id),
                        400,
                    ));
                }
                (
                    self.store()
                        .team()
                        .update_member(&candidate)
                        .await
                        .map_err(save_member_error)?,
                    false,
                )
            }
        };

        if already_added {
            return Ok(member);
        }

        // `UpdateUpdateAt` would go here — see the note above (D-242).

        if let Err(err) = self
            .create_initial_sidebar_categories(&user.id, &team.id)
            .await
        {
            tracing::warn!(
                user_id = %user.id,
                team_id = %team.id,
                error = %err,
                "Encountered an issue creating default sidebar categories."
            );
        }

        let should_be_admin = team.email == user.email;

        if !is_guest
            && let Err(err) = self
                .join_default_channels(&team.id, user, should_be_admin, user_requestor_id)
                .await
        {
            tracing::warn!(
                user_id = %user.id,
                team_id = %team.id,
                error = %err,
                "Encountered an issue joining default channels."
            );
        }

        self.publish_added_to_team(&team.id, &user.id).await;

        Ok(member)
    }

    /// Port of `app.App.JoinDefaultChannels` (app/channel.go:59).
    ///
    /// # The `SaveMember` error is inspected **after** the loop, not inside it
    ///
    /// Go assigns to the shared `nErr` and carries on, so a failure on `town-square` does not stop
    /// `off-topic`, and only the **last** channel's error is ever reported. The history write is
    /// the opposite — it returns immediately. Reproduced exactly: swapping the two would change
    /// which channels a half-failed join leaves a user in.
    ///
    /// # A channel that is missing is skipped, a channel that is not open is skipped
    ///
    /// `GetByName` failing is a log line and a `continue`; `channel.Type != Open` is a silent
    /// `continue`. So on a team whose `town-square` was converted to private, a new member simply
    /// does not get it.
    ///
    /// # The requestor lookup is the one hard failure before the loop
    ///
    /// And it runs only when `userRequestorId != ""` — which is the single-add route's `""`
    /// versus the batch route's session id. Its only *use* in Go is the join system post, which
    /// this port does not write (D-243), but the lookup and its error branch are on the path
    /// regardless and are kept.
    ///
    /// # The `user_added` event here is not `AddChannelMember`'s
    ///
    /// One event, addressed to the **channel**, with `user_id` and `team_id` and **no
    /// `omit_users`** — where `AddChannelMember` publishes two and omits the added user from the
    /// channel-addressed one. So a user joining a team *does* see their own `user_added`.
    #[tracing::instrument(skip(self, user), fields(team_id = %team_id, user_id = %user.id, should_be_admin))]
    pub async fn join_default_channels(
        &self,
        team_id: &str,
        user: &User,
        should_be_admin: bool,
        user_requestor_id: &str,
    ) -> AppResult<()> {
        if !user_requestor_id.is_empty() {
            self.store()
                .user()
                .get(user_requestor_id)
                .await
                .map_err(|err| match err.is_not_found() {
                    true => AppError::boxed(
                        "JoinDefaultChannels",
                        MISSING_ACCOUNT_ERROR,
                        None,
                        String::new(),
                        404,
                    ),
                    false => {
                        tracing::error!(error = %err, "requestor lookup failed");
                        AppError::boxed(
                            "JoinDefaultChannels",
                            "app.user.get.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                })?;
        }

        let is_guest = user.is_guest();
        let mut last_save_error: Option<mm_store::StoreError> = None;

        for channel_name in self.default_channel_names() {
            let channel = match self
                .store()
                .channel()
                .get_by_name(team_id, &channel_name, true)
                .await
            {
                Ok(channel) => channel,
                Err(err) => {
                    tracing::warn!(
                        channel_name = %channel_name,
                        team_id = %team_id,
                        error = %err,
                        "No default channel with this name"
                    );
                    continue;
                }
            };

            if channel.channel_type != CHANNEL_TYPE_OPEN {
                continue;
            }

            let member = ChannelMember {
                channel_id: channel.id.clone(),
                user_id: user.id.clone(),
                scheme_guest: is_guest,
                scheme_user: !is_guest,
                scheme_admin: should_be_admin,
                notify_props: Some(get_default_channel_notify_props()),
                ..ChannelMember::default()
            };

            if let Err(err) = self.store().channel().save_member(member).await {
                last_save_error = Some(err);
            }

            self.store()
                .channel_member_history()
                .log_join_event(&user.id, &channel.id, get_millis())
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "join history write failed");
                    AppError::boxed(
                        "JoinDefaultChannels",
                        "app.channel_member_history.log_join_event.internal_error",
                        None,
                        String::new(),
                        500,
                    )
                })?;

            // The join system post would go here — D-243.

            let mut event =
                WebSocketEvent::new(WEBSOCKET_EVENT_USER_ADDED, "", &channel.id, "", None, "");
            event.add("user_id", serde_json::Value::String(user.id.clone()));
            event.add(
                "team_id",
                serde_json::Value::String(channel.team_id.clone()),
            );
            self.publish(event).await;
        }

        match last_save_error {
            None => Ok(()),
            Some(mm_store::StoreError::Conflict {
                resource: "ChannelMembers",
                ..
            }) => Err(AppError::boxed(
                "JoinDefaultChannels",
                "app.channel.save_member.exists.app_error",
                None,
                String::new(),
                400,
            )),
            Some(mm_store::StoreError::Invalid { app_error, .. }) => Err(app_error),
            Some(err) => {
                tracing::error!(error = %err, "default channel member save failed");
                Err(AppError::boxed(
                    "JoinDefaultChannels",
                    // Go's id, and it names the wrong thing — `create_direct_channel` for a
                    // default-channel join. Copied rather than corrected.
                    "app.channel.create_direct_channel.internal_error",
                    None,
                    String::new(),
                    500,
                ))
            }
        }
    }

    /// Port of `app.App.DefaultChannelNames` (app/channel.go:41).
    ///
    /// `town-square` is always first. An empty `ExperimentalDefaultChannels` appends `off-topic`;
    /// a configured list **replaces `off-topic`** and is de-duplicated against a seed set that
    /// already holds `town-square` — so naming `town-square` in the setting does not double it,
    /// and naming `off-topic` there is the only way to keep it alongside others.
    #[must_use]
    pub fn default_channel_names(&self) -> Vec<String> {
        let configured = &self.config().experimental_default_channels;
        if configured.is_empty() {
            return vec!["town-square".to_owned(), "off-topic".to_owned()];
        }

        let mut names = vec!["town-square".to_owned()];
        let mut seen: Vec<&str> = vec!["town-square"];
        for name in configured {
            if !seen.contains(&name.as_str()) {
                names.push(name.clone());
                seen.push(name);
            }
        }
        names
    }

    /// Port of `TeamService.IsTeamEmailAllowed` (app/teams/utils.go:60).
    ///
    /// **A bot is always allowed**, before the address is even looked at. Otherwise the address is
    /// lower-cased and tested against a *list of restrictions*, each of which is a free-form
    /// string: a guest gets `GuestAccountsSettings.RestrictCreationToDomains` alone, and everyone
    /// else gets the **team's** `AllowedDomains` followed by
    /// `TeamSettings.RestrictCreationToDomains`.
    ///
    /// Every non-empty restriction must be satisfied — see [`is_email_address_allowed`] — so the
    /// two lists are an *intersection*, not a union. A team that allows `example.com` on a server
    /// restricted to `corp.example.com` admits nobody.
    #[must_use]
    pub fn is_team_email_allowed(&self, user: &User, team: &Team) -> bool {
        if user.is_bot {
            return true;
        }
        let email = user.email.to_lowercase();
        if user.is_guest() {
            return is_email_address_allowed(
                &email,
                &[&self.config().guest_restrict_creation_to_domains],
            );
        }
        is_email_address_allowed(
            &email,
            &[
                &team.allowed_domains,
                &self.config().restrict_creation_to_domains,
            ],
        )
    }

    /// Port of `app.App.TeamAccessControlled` (team.go:949).
    ///
    /// [`App::team_membership_access_control_enabled`] is a constant `false` on this deployment —
    /// Team Edition, no licence — so this is a constant `false` too and the `HydrateTeamPolicyActions`
    /// branch below it is unreachable. Kept as a function so the *call sites* read like Go's and
    /// so pointing the licence check at a real read makes them all correct at once.
    #[must_use]
    pub fn team_access_controlled(&self, _team_id: &str) -> bool {
        self.team_membership_access_control_enabled()
    }

    /// `model.NewWebSocketEvent(WebsocketEventAddedToTeam, "", "", userID, nil, "")` with
    /// `team_id` and `user_id` in the payload — addressed to the **joining user**, so nobody else
    /// on the team is told.
    async fn publish_added_to_team(&self, team_id: &str, user_id: &str) {
        let mut event =
            WebSocketEvent::new(WEBSOCKET_EVENT_ADDED_TO_TEAM, "", "", user_id, None, "");
        event.add("team_id", serde_json::Value::String(team_id.to_owned()));
        event.add("user_id", serde_json::Value::String(user_id.to_owned()));
        self.publish(event).await;
    }
}

/// Go's `MissingAccountError` (app/constants.go:7) — a literal string that happens to look like a
/// translation key and is not one.
const MISSING_ACCOUNT_ERROR: &str = "app.user.missing_account.const";

/// Port of `teams.IsEmailAddressAllowed` (app/teams/utils.go:38).
///
/// **Every non-empty restriction must match**, and an empty one is skipped — so the default
/// (`""` everywhere) admits everybody, and two configured lists must *both* accept the address.
/// The comparison is `strings.HasSuffix(email, "@"+domain)`. The leading `@` is what keeps it
/// honest in both directions: `example.com` does **not** admit `me@sub.example.com` (the character
/// before `example.com` is a dot, not an `@`), and it does not admit `me@notexample.com` either.
/// Dropping the `@` from the suffix would admit both.
fn is_email_address_allowed(email: &str, restrictions: &[&str]) -> bool {
    for restriction in restrictions {
        let domains = normalize_domains(restriction);
        if domains.is_empty() {
            continue;
        }
        if !domains
            .iter()
            .any(|domain| email.ends_with(&format!("@{domain}")))
        {
            return false;
        }
    }
    true
}

/// Port of `teams.normalizeDomains` (app/teams/utils.go:91).
///
/// Lower-case, then `@` and `,` both become spaces, then `strings.Fields`. So
/// `"@corp.example.com, example.com  example.org"` is three domains, and a stray `@` in the middle
/// of a name splits it in two rather than being stripped.
fn normalize_domains(domains: &str) -> Vec<String> {
    domains
        .to_lowercase()
        .replace(['@', ','], " ")
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// The `app.team.join_user_to_team.*` family, all with `JoinUserToTeam` as the `where`.
fn join_error(id: &'static str, details: String, status: i32) -> Box<AppError> {
    AppError::boxed("JoinUserToTeam", id, None, details, status)
}

/// Go's `switch` over the store error in `App.JoinUserToTeam` (team.go:822), in Go's order.
///
/// `errors.As(err, &appErr)` comes **before** the conflict and limit arms, so an `IsValid`
/// failure keeps its own id and 400 rather than being relabelled.
fn save_member_error(err: mm_store::StoreError) -> Box<AppError> {
    match err {
        mm_store::StoreError::Invalid { app_error, .. } => app_error,
        mm_store::StoreError::Conflict { .. } => join_error(
            "app.team.join_user_to_team.save_member.conflict.app_error",
            String::new(),
            400,
        ),
        mm_store::StoreError::LimitExceeded { .. } => join_error(
            "app.team.join_user_to_team.save_member.max_accounts.app_error",
            String::new(),
            400,
        ),
        other => {
            tracing::error!(error = %other, "team member save failed");
            join_error(
                "app.team.join_user_to_team.save_member.app_error",
                String::new(),
                500,
            )
        }
    }
}

impl App {
    /// Port of `app.App.RemoveUserFromTeam` (app/team.go:1240).
    ///
    /// Go runs the team and user reads in **parallel goroutines** and then joins on the team
    /// first, so the team's error wins when both fail. Sequential here, in that same order, which
    /// is the only part of the concurrency that is observable.
    ///
    /// # The team's not-found id is a copy-paste
    ///
    /// Both arms of the team read answer `app.team.get_by_invite_id.finding.app_error` — an id
    /// about *invite ids* on a plain `Team().Get` — and they differ only in status: **404** for
    /// not-found, **500** otherwise. Reproduced verbatim; a client matching on the id would break
    /// if this were "corrected" to `app.team.get.find.app_error`.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, user_id = %user_id))]
    pub async fn remove_user_from_team(
        &self,
        team_id: &str,
        user_id: &str,
        requestor_id: &str,
    ) -> AppResult<()> {
        let team = self.store().team().get(team_id).await.map_err(|err| {
            let status = if err.is_not_found() { 404 } else { 500 };
            if status == 500 {
                tracing::error!(error = %err, "team lookup failed");
            }
            AppError::boxed(
                "RemoveUserFromTeam",
                "app.team.get_by_invite_id.finding.app_error",
                None,
                String::new(),
                status,
            )
        })?;

        let user = self.store().user().get(user_id).await.map_err(|err| {
            if err.is_not_found() {
                AppError::boxed(
                    "RemoveUserFromTeam",
                    MISSING_ACCOUNT_ERROR,
                    None,
                    String::new(),
                    404,
                )
            } else {
                tracing::error!(error = %err, "user lookup failed");
                AppError::boxed(
                    "RemoveUserFromTeam",
                    "app.user.get.app_error",
                    None,
                    String::new(),
                    500,
                )
            }
        })?;

        self.leave_team(&team, &user, requestor_id).await
    }

    /// Port of `app.App.LeaveTeam` (app/team.go:1331) — the cascade a team departure runs.
    ///
    /// # Order of operations, and every step's failure mode
    ///
    /// 1. `GetTeamMember`. **Any** failure, not-found included, is
    ///    `api.team.remove_user_from_team.missing.app_error` at **400** — so removing someone who
    ///    is not on the team is a 400 and not a 404.
    /// 2. `GetChannels(team, user, {include_deleted: true})`. `ErrNotFound` — which this store
    ///    raises for an **empty** result — is an empty list, not an error. Getting that backwards
    ///    makes every removal of a channel-less member a 500.
    /// 3. Each channel that is **not** a DM or GM: cache invalidation, then
    ///    `removeChannelMembership`. So a departing member keeps their DMs, which is why the
    ///    predicate is `IsGroupOrDirect` and not "is on this team".
    /// 4. The team's **space** channels, which step 2 cannot see: `messageChannelTypes` excludes
    ///    `S`. Their membership rows would otherwise survive the leave and keep authorising
    ///    space-scoped websocket delivery — see
    ///    [`mm_store::channel_store::get_team_space_channels_for_user`].
    /// 5. `ExperimentalEnableDefaultChannelLeaveJoinMessages`, which defaults to **true**: read
    ///    `town-square` by name — **and its failure fails the whole removal**, 404 or 500 — then
    ///    post one of two system messages. `requestorId == user.Id` picks "left the team", any
    ///    other requestor picks "removed from the team"; both are logged and swallowed.
    /// 6. `RemoveTeamMember`, the membership write and its two websocket events.
    /// 7. `postProcessTeamMemberLeave`.
    ///
    /// Steps 1-5 read and write **channel** state before the membership row is touched, so a
    /// failure in the middle leaves a user in the team and out of its channels. Go's.
    ///
    /// # What is deliberately not here
    ///
    /// The ABAC audit records (`policyDriven` is `team.PolicyEnforced && requestorId == ""`, and
    /// this route always passes the session's user id, so it is false), the plugin hook
    /// ([D-183]), and the three cache invalidations this server has no caches for ([D-190]).
    #[tracing::instrument(skip(self, team, user), fields(team_id = %team.id, user_id = %user.id))]
    pub async fn leave_team(&self, team: &Team, user: &User, requestor_id: &str) -> AppResult<()> {
        let mut member = self
            .store()
            .team()
            .get_member(&team.id, &user.id)
            .await
            .map_err(|err| {
                tracing::debug!(error = %err, "team membership lookup failed");
                AppError::boxed(
                    "LeaveTeam",
                    "api.team.remove_user_from_team.missing.app_error",
                    None,
                    String::new(),
                    400,
                )
            })?;

        let opts = mm_model::channel::ChannelSearchOpts {
            include_deleted: true,
            last_delete_at: 0,
            ..Default::default()
        };
        let channels = match self
            .store()
            .channel()
            .get_channels(&team.id, &user.id, &opts)
            .await
        {
            Ok(list) => list.0,
            // Go's `errors.As(nErr, &nfErr)` arm: an empty membership list, not a failure.
            Err(err) if err.is_not_found() => Vec::new(),
            Err(err) => {
                tracing::error!(error = %err, "channel listing failed");
                return Err(AppError::boxed(
                    "LeaveTeam",
                    "app.channel.get_channels.get.app_error",
                    None,
                    String::new(),
                    500,
                ));
            }
        };

        for channel in &channels {
            if !channel.is_group_or_direct() {
                self.remove_channel_membership(&user.id, &channel.id)
                    .await?;
            }
        }

        let space_channels = self
            .store()
            .channel()
            .get_team_space_channels_for_user(&team.id, &user.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "space channel listing failed");
                AppError::boxed(
                    "LeaveTeam",
                    "app.channel.get_channels.get.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        for channel in &space_channels.0 {
            self.remove_channel_membership(&user.id, &channel.id)
                .await?;
        }

        if self
            .config()
            .experimental_enable_default_channel_leave_join_messages
        {
            let channel = self
                .store()
                .channel()
                .get_by_name(&team.id, DEFAULT_CHANNEL_NAME, false)
                .await
                .map_err(|err| {
                    if err.is_not_found() {
                        AppError::boxed(
                            "LeaveTeam",
                            "app.channel.get_by_name.missing.app_error",
                            None,
                            String::new(),
                            404,
                        )
                    } else {
                        tracing::error!(error = %err, "town-square lookup failed");
                        AppError::boxed(
                            "LeaveTeam",
                            "app.channel.get_by_name.existing.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                })?;

            self.post_team_leave_message(user, &channel, requestor_id == user.id)
                .await;
        }

        // `&mut`, matching Go's pointer: `RemoveTeamMember` stamps `DeleteAt` on the struct
        // and `postProcessTeamMemberLeave` then reads the *mutated* one. It only reads `UserId`
        // and `TeamId`, which do not move, so the sharing is not observable — but a clone here
        // would quietly make it unobservable by construction.
        self.remove_team_member(&mut member).await?;
        self.post_process_team_member_leave(&member).await
    }

    /// The two system posts of `LeaveTeam` — `postLeaveTeamMessage` (team.go:1440) and
    /// `postRemoveFromTeamMessage` (team.go:1459).
    ///
    /// One function because they differ in exactly two values: the message text and the post
    /// type. **`self_leave` is `requestorId == user.Id`**, so an admin removing themselves posts
    /// "left" and an admin removing someone else posts "removed" — the distinction a reader is
    /// most likely to collapse, and it is the post body every member of the team then sees.
    ///
    /// Go's translations are `"%v left the team."` and `"%v removed from the team."` — note the
    /// second has no "was". Both are `i18n.T` with the **server's** locale, not the acting user's.
    ///
    /// Go swallows both errors (`rctx.Logger().Warn`), so [`App::post_system_message`] is the
    /// right wrapper: a post that cannot be written does not fail the removal.
    async fn post_team_leave_message(
        &self,
        user: &User,
        channel: &mm_model::channel::Channel,
        self_leave: bool,
    ) {
        let (message, post_type) = if self_leave {
            (
                format!("{} left the team.", user.username),
                mm_model::post::POST_TYPE_LEAVE_TEAM,
            )
        } else {
            (
                format!("{} removed from the team.", user.username),
                mm_model::post::POST_TYPE_REMOVE_FROM_TEAM,
            )
        };

        let mut post = mm_model::post::Post {
            channel_id: channel.id.clone(),
            message,
            post_type: post_type.to_owned(),
            user_id: user.id.clone(),
            ..Default::default()
        };
        post.add_prop("username", serde_json::Value::String(user.username.clone()));

        self.post_system_message(post, channel).await;
    }

    /// Port of `TeamService.RemoveTeamMember` (app/teams/teams.go).
    ///
    /// # Two websocket events, and they are published **before** the write
    ///
    /// One addressed to the **team** omitting the departing user, one addressed to the **user**
    /// alone — so the person being removed is told exactly once, on their own connection, and
    /// does not also receive the team broadcast. Both carry `user_id` and `team_id`.
    ///
    /// Publishing first means a failed write leaves every client believing the removal happened.
    /// Go's order; moving the events after the write would be the safer code and the wrong port.
    ///
    /// # The row is soft-deleted, and the roles clear is a **no-op on the database**
    ///
    /// `Roles = ""` and `DeleteAt = now`, but `UpdateMember` writes `ExplicitRoles` into the
    /// `Roles` column — `member.roles` is the *computed* field and is never persisted by either
    /// server. So the assignment changes only the struct that is about to be dropped, and a
    /// departed member still reads back as `team_user` because `SchemeUser` is untouched.
    /// Measured against Go, which does exactly the same thing (`NewTeamMemberFromModel` maps
    /// `ExplicitRoles` to the column). Kept because it is what the source says; a mutation that
    /// deletes this line is **equivalent**, not a gap.
    #[tracing::instrument(skip(self, member), fields(team_id = %member.team_id, user_id = %member.user_id))]
    async fn remove_team_member(&self, member: &mut TeamMember) -> AppResult<()> {
        let mut to_team = WebSocketEvent::new(
            WEBSOCKET_EVENT_LEAVE_TEAM,
            &member.team_id,
            "",
            "",
            // `omitUsers` is Go's `map[string]bool{userId: true}`: the departing user is *not*
            // told through the team broadcast, only through the personal event below.
            Some(std::collections::BTreeMap::from([(
                member.user_id.clone(),
                true,
            )])),
            "",
        );
        to_team.add("user_id", serde_json::Value::String(member.user_id.clone()));
        to_team.add("team_id", serde_json::Value::String(member.team_id.clone()));
        self.publish(to_team).await;

        let mut to_user = WebSocketEvent::new(
            WEBSOCKET_EVENT_LEAVE_TEAM,
            "",
            "",
            &member.user_id,
            None,
            "",
        );
        to_user.add("user_id", serde_json::Value::String(member.user_id.clone()));
        to_user.add("team_id", serde_json::Value::String(member.team_id.clone()));
        self.publish(to_user).await;

        member.roles = String::new();
        member.delete_at = get_millis();

        self.store()
            .team()
            .update_member(member)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "team membership update failed");
                // Go's `Where` is `RemoveTeamMemberFromTeam`, not `LeaveTeam` — invisible on the
                // wire (`Where` is `json:"-"`), kept because it is what the source says.
                AppError::boxed(
                    "RemoveTeamMemberFromTeam",
                    "app.team.save_member.save.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        Ok(())
    }

    /// Port of `app.App.postProcessTeamMemberLeave` (app/team.go:1286).
    ///
    /// Three writes, each one a 500 with its own id, in Go's order: the user's `UpdateAt`, the
    /// **sidebar** rows for that team, and every preference in the team's category. The user is
    /// re-read first purely so the `MissingAccountError` branch exists; the id is already in hand.
    ///
    /// The sidebar and preference deletes are what stop a removed-and-re-added member seeing a
    /// stale sidebar and a stale "last channel viewed" — and neither has any other trigger, so
    /// dropping one is invisible until a rejoin.
    ///
    /// The plugin hook and the three cache invalidations are [D-183] and [D-190].
    #[tracing::instrument(skip(self, member), fields(team_id = %member.team_id, user_id = %member.user_id))]
    async fn post_process_team_member_leave(&self, member: &TeamMember) -> AppResult<()> {
        let user = self
            .store()
            .user()
            .get(&member.user_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "postProcessTeamMemberLeave",
                        MISSING_ACCOUNT_ERROR,
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "user lookup failed");
                    AppError::boxed(
                        "postProcessTeamMemberLeave",
                        "app.user.get.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        self.store()
            .user()
            .update_update_at(&user.id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "user update_at bump failed");
                AppError::boxed(
                    "postProcessTeamMemberLeave",
                    "app.user.update_update.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        self.store()
            .sidebar_category()
            .clear_sidebar_on_team_leave(&user.id, &member.team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "sidebar clear failed");
                AppError::boxed(
                    "postProcessTeamMemberLeave",
                    "app.channel.sidebar_categories.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        // **The team id is the preference category.** Go's comment: "delete the preferences that
        // set the last channel used in the team and other team specific preferences".
        self.store()
            .preference()
            .delete_category(&user.id, &member.team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "preference category delete failed");
                AppError::boxed(
                    "postProcessTeamMemberLeave",
                    "app.preference.delete.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use mm_store::SqlStore;
    use sqlx::postgres::PgPoolOptions;

    /// An `App` that can answer config questions and nothing else. The 250ms cap is the standing
    /// one: sqlx's default `acquire_timeout` is 30 seconds and six tests once sat on it.
    fn app_with(config: Config) -> App {
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        App::with_config(SqlStore::from_pool(pool), config)
    }

    /// `DefaultChannelNames`: `town-square` always, `off-topic` only when the setting is empty,
    /// and the configured list de-duplicated against a seed that already holds `town-square`.
    #[tokio::test]
    async fn default_channel_names_match_gos_three_branches() {
        assert_eq!(
            app_with(Config::default()).default_channel_names(),
            vec!["town-square", "off-topic"],
            "the stock server joins both"
        );

        let configured = |names: &[&str]| Config {
            experimental_default_channels: names.iter().map(|n| (*n).to_owned()).collect(),
            ..Config::default()
        };

        assert_eq!(
            app_with(configured(&["welcome"])).default_channel_names(),
            vec!["town-square", "welcome"],
            "a configured list replaces off-topic, it does not extend it"
        );
        assert_eq!(
            app_with(configured(&["town-square", "welcome"])).default_channel_names(),
            vec!["town-square", "welcome"],
            "naming town-square does not double it"
        );
        assert_eq!(
            app_with(configured(&["welcome", "welcome"])).default_channel_names(),
            vec!["town-square", "welcome"],
            "and the seen-set applies to the configured names too"
        );
        assert_eq!(
            app_with(configured(&["off-topic"])).default_channel_names(),
            vec!["town-square", "off-topic"],
            "off-topic survives only by being named"
        );
    }

    /// `IsTeamEmailAllowed`'s two short-circuits, which the domain corpus below cannot reach: a
    /// **bot** is allowed before the address is read, and a **guest** is tested against the guest
    /// restriction alone rather than against the team's `AllowedDomains`.
    #[tokio::test]
    async fn the_team_email_gate_short_circuits_for_bots_and_narrows_for_guests() {
        let app = app_with(Config {
            restrict_creation_to_domains: "example.com".to_owned(),
            guest_restrict_creation_to_domains: "guests.example.com".to_owned(),
            ..Config::default()
        });
        let team = Team {
            allowed_domains: "example.com".to_owned(),
            ..Team::default()
        };

        let outsider = User {
            email: "me@nope.com".to_owned(),
            ..User::default()
        };
        assert!(!app.is_team_email_allowed(&outsider, &team));

        let bot = User {
            email: "me@nope.com".to_owned(),
            is_bot: true,
            ..User::default()
        };
        assert!(app.is_team_email_allowed(&bot, &team), "a bot is exempt");

        // A guest is measured against `GuestAccountsSettings.RestrictCreationToDomains` **only**,
        // so an address the team would accept is refused and one it would not is admitted.
        let guest_ok = User {
            email: "g@guests.example.com".to_owned(),
            roles: "system_guest".to_owned(),
            ..User::default()
        };
        assert!(guest_ok.is_guest(), "the fixture really is a guest");
        assert!(app.is_team_email_allowed(&guest_ok, &team));

        let guest_on_the_team_domain = User {
            email: "g@example.com".to_owned(),
            roles: "system_guest".to_owned(),
            ..User::default()
        };
        assert!(
            !app.is_team_email_allowed(&guest_on_the_team_domain, &team),
            "the team's AllowedDomains is not consulted for a guest"
        );
    }

    /// `teams.IsEmailAddressAllowed` against the generated corpus — the AND across restrictions,
    /// the skip for an empty one, and the `@` inside the suffix.
    mod go_parity {
        use super::*;

        #[derive(serde::Deserialize)]
        struct EmailCase {
            name: String,
            email: String,
            restrictions: Vec<String>,
            allowed: bool,
        }

        #[test]
        fn is_email_address_allowed_matches_go() {
            let raw = include_str!("../../../fixtures/behaviour_team_email.json");
            let corpus: serde_json::Value = serde_json::from_str(raw).unwrap();
            let cases: Vec<EmailCase> =
                serde_json::from_value(corpus["is_email_address_allowed"].clone()).unwrap();
            assert!(cases.len() >= 25, "the corpus is the oracle; keep it broad");

            for case in cases {
                let restrictions: Vec<&str> =
                    case.restrictions.iter().map(String::as_str).collect();
                assert_eq!(
                    is_email_address_allowed(&case.email, &restrictions),
                    case.allowed,
                    "{}: {:?} against {:?}",
                    case.name,
                    case.email,
                    case.restrictions
                );
            }
        }
    }

    /// The four ids two routes share, spelled out so a rename has to touch a test. Three say
    /// `api.team`; `changing_guest_role` says `api.channel` and that is deliberate.
    #[test]
    fn the_error_ids_match_gos_including_the_channel_prefixed_one() {
        assert_eq!(
            roles_error("api.team", "guest_and_user").id,
            "api.team.update_team_member_roles.guest_and_user.app_error"
        );
        assert_eq!(
            roles_error("api.team", "unset_user_scheme").id,
            "api.team.update_team_member_roles.unset_user_scheme.app_error"
        );
        assert_eq!(
            roles_error("api.channel", "changing_guest_role").id,
            "api.channel.update_team_member_roles.changing_guest_role.app_error"
        );
        assert_eq!(
            scheme_role_error("team_post_all").id,
            "api.channel.update_team_member_roles.scheme_role.app_error"
        );
        assert_eq!(
            scheme_role_error("team_post_all").detailed_error,
            "role_name=team_post_all"
        );
        for family in ["api.team", "api.channel"] {
            assert_eq!(roles_error(family, "guest").status_code, 400);
        }
    }

    /// `removeRoles` drops the named roles and keeps the rest in order; a role that is a
    /// *prefix* of one being removed survives, because the comparison is whole-token.
    #[test]
    fn remove_roles_drops_only_whole_tokens() {
        assert_eq!(
            remove_roles(
                &[TEAM_GUEST_ROLE_ID, TEAM_USER_ROLE_ID, TEAM_ADMIN_ROLE_ID],
                "custom_one team_user custom_two team_admin"
            ),
            "custom_one custom_two"
        );
        assert_eq!(
            remove_roles(&[TEAM_USER_ROLE_ID], "team_user_extra"),
            "team_user_extra"
        );
        assert_eq!(remove_roles(&[TEAM_USER_ROLE_ID], "  team_user   "), "");
        assert_eq!(
            remove_roles(&[], "a  b"),
            "a b",
            "runs of whitespace collapse"
        );
    }
}
