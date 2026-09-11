//! Port of the team-membership **write** paths of `server/channels/app/team.go`.
//!
//! | route | app entry point |
//! |---|---|
//! | `PUT /teams/{id}/members/{user}/roles` | [`App::update_team_member_roles`] |
//! | `PUT /teams/{id}/members/{user}/schemeRoles` | [`App::update_team_member_scheme_roles`] |
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
use mm_model::channel_member::{ChannelMember, get_default_channel_notify_props};
use mm_model::role::{TEAM_ADMIN_ROLE_ID, TEAM_GUEST_ROLE_ID, TEAM_USER_ROLE_ID};
use mm_model::team::Team;
use mm_model::team_member::{TeamMember, TeamMemberWithError};
use mm_model::user::User;
use mm_model::utils::{AppError, AppResult, get_millis};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_ADDED_TO_TEAM, WEBSOCKET_EVENT_MEMBERROLE_UPDATED, WEBSOCKET_EVENT_USER_ADDED,
    WebSocketEvent,
};
use mm_store::{ChannelMemberHistoryStore, ChannelStore, GroupStore, TeamStore, UserStore};

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
    async fn send_updated_team_member_event(&self, member: &TeamMember) {
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
    ///   `update_at` after a join this server served. Recorded as **D-233**.
    /// - **The join system post.** `ExperimentalEnableDefaultChannelLeaveJoinMessages` defaults
    ///   to **`true`** (config.go:874), so a stock Go server *does* post "user joined the team"
    ///   in `town-square`. This port writes no `Posts` rows — **D-231/D-234**.
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

        // `UpdateUpdateAt` would go here — see the note above (D-233).

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
    /// this port does not write (D-234), but the lookup and its error branch are on the path
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

            // The join system post would go here — D-234.

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

#[cfg(test)]
mod tests {
    use super::*;

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
