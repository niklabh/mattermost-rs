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

use mm_model::role::{TEAM_ADMIN_ROLE_ID, TEAM_GUEST_ROLE_ID, TEAM_USER_ROLE_ID};
use mm_model::team_member::TeamMember;
use mm_model::utils::{AppError, AppResult};
use mm_model::websocket_message::{WEBSOCKET_EVENT_MEMBERROLE_UPDATED, WebSocketEvent};
use mm_store::TeamStore;

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
