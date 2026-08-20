//! Port of the team app-layer surface (channels/app/team.go): `GetTeamMembersForUser`,
//! `GetTeamsForUser`, and the `SanitizeTeam`/`SanitizeTeams` pair.

use mm_model::permission::{PERMISSION_INVITE_USER, PERMISSION_MANAGE_TEAM};
use mm_model::session::Session;
use mm_model::team::Team;
use mm_model::team_member::TeamMember;
use mm_model::utils::AppError;
use mm_store::TeamStore;

use crate::App;

impl App {
    /// Port of `app.App.GetTeamMembersForUser` (team.go:1108).
    ///
    /// A thin wrapper: Go's whole body is the store call plus one error mapping, and *any* store
    /// failure becomes `app.team.get_members.app_error` with a 500. There is no not-found branch,
    /// because a user in no teams is an empty list rather than a miss.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn get_team_members_for_user(
        &self,
        user_id: &str,
        exclude_team_id: &str,
        include_deleted: bool,
    ) -> Result<Vec<TeamMember>, AppError> {
        self.store()
            .team()
            .get_teams_for_user(user_id, exclude_team_id, include_deleted)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "team members lookup failed");
                AppError::new(
                    "GetTeamMembersForUser",
                    "app.team.get_members.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetTeam` (team.go:897), through the `TeamService.GetTeam` pass-through
    /// (app/teams/teams.go:30) it delegates to.
    ///
    /// Same two-branch shape as `App::get_channel`, with a transcription trap in the ids: the
    /// 404 is `app.team.get.find.app_error` and the 500 is `app.team.get.finding.app_error` —
    /// **`find` versus `finding`**, one gerund apart, where the channel pair varies the whole
    /// last word (`existing`/`find`). Neither branch carries `params`; Go passes `nil` here,
    /// unlike `GetChannel`'s `errCtx`.
    #[tracing::instrument(skip_all, fields(team_id = %team_id))]
    pub async fn get_team(&self, team_id: &str) -> Result<Team, AppError> {
        self.store().team().get(team_id).await.map_err(|err| {
            if err.is_not_found() {
                AppError::new(
                    "GetTeam",
                    "app.team.get.find.app_error",
                    None,
                    String::new(),
                    404,
                )
            } else {
                tracing::error!(error = %err, "team lookup failed");
                AppError::new(
                    "GetTeam",
                    "app.team.get.finding.app_error",
                    None,
                    String::new(),
                    500,
                )
            }
        })
    }

    /// Port of `app.App.GetTeamStats` (team.go:2234), restrictions-free — the caller forwards
    /// any restricted request to Go, so this port never sees a `ViewUsersRestrictions`.
    ///
    /// Go launches both counts on goroutines and then reads the **total**'s channel first, so
    /// when both fail the total's error is the one reported. Sequential awaits preserve exactly
    /// that precedence; the concurrency itself is invisible on the wire. The two error ids
    /// differ only by an inserted `active_` — `app.team.get_member_count.app_error` versus
    /// `app.team.get_active_member_count.app_error` — and the first is also the id
    /// `GetChannelGuestCount` borrows in `channel.rs`, so three call sites now share it.
    #[tracing::instrument(skip_all, fields(team_id = %team_id))]
    pub async fn get_team_stats(
        &self,
        team_id: &str,
    ) -> Result<mm_model::stats::TeamStats, AppError> {
        let total_member_count = self
            .store()
            .team()
            .get_total_member_count(team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "total member count failed");
                AppError::new(
                    "GetTeamStats",
                    "app.team.get_member_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let active_member_count = self
            .store()
            .team()
            .get_active_member_count(team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "active member count failed");
                AppError::new(
                    "GetTeamStats",
                    "app.team.get_active_member_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(mm_model::stats::TeamStats {
            team_id: team_id.to_owned(),
            total_member_count,
            active_member_count,
        })
    }

    /// Port of `app.App.GetTeamsForUser` (team.go:1084).
    ///
    /// Same thin-wrapper shape as [`App::get_team_members_for_user`], different store read and
    /// error id: any failure is `app.team.get_all.app_error` at 500, and a user in no teams is
    /// an empty list, not a miss.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn get_teams_for_user(&self, user_id: &str) -> Result<Vec<Team>, AppError> {
        self.store()
            .team()
            .get_teams_by_user_id(user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "teams lookup failed");
                AppError::new(
                    "GetTeamsForUser",
                    "app.team.get_all.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.SanitizeTeam` (team.go:2303).
    ///
    /// **Both permission checks run unconditionally, before any branch** — Go computes
    /// `manageTeamPermission` and `inviteUserPermission` up front rather than short-circuiting,
    /// so this port does too; lazier evaluation would change which database reads happen, not
    /// the output. The field logic is the part a reader could get wrong:
    ///
    /// - both permissions → the team is untouched;
    /// - otherwise `Sanitize()` clears **both** `email` and `invite_id`, and each permission
    ///   restores only its own field: `manage_team` gets `email` back, `invite_user` gets
    ///   `invite_id` back. Crossing that pairing leaks an invite id to someone who can merely
    ///   manage settings, and an invite id is enough to join the team — the exact leak [D-094]
    ///   kept this route forwarded for.
    ///
    /// Owned `&mut Team` instead of Go's pointer-in-pointer-out; the caller keeps the list.
    #[tracing::instrument(skip_all, fields(team_id = %team.id))]
    pub async fn sanitize_team(&self, session: &Session, team: &mut Team) {
        let manage_team_permission = self
            .session_has_permission_to_team(session, &team.id, &PERMISSION_MANAGE_TEAM)
            .await;
        let invite_user_permission = self
            .session_has_permission_to_team(session, &team.id, &PERMISSION_INVITE_USER)
            .await;

        apply_team_sanitize(team, manage_team_permission, invite_user_permission);
    }

    /// Port of `app.App.SanitizeTeams` (team.go:2323) — every team, in place, in order.
    pub async fn sanitize_teams(&self, session: &Session, teams: &mut [Team]) {
        for team in teams.iter_mut() {
            self.sanitize_team(session, team).await;
        }
    }
}

/// The field half of `SanitizeTeam` (team.go:2307-2320), lifted out of the two permission reads
/// so the **pairing** can be pinned without a database: `manage_team` restores `email`,
/// `invite_user` restores `invite_id`. Crossing that pairing hands an invite id — enough to join
/// the team — to someone who merely holds `manage_team`, which is the exact leak [D-094] kept
/// this route forwarded for. Same lift as `apply_channel_mentions_prop`.
///
/// With both permissions the team is returned untouched — not sanitised-and-restored — so any
/// *other* field `Sanitize` might someday clear also survives; that early return is Go's.
fn apply_team_sanitize(
    team: &mut Team,
    manage_team_permission: bool,
    invite_user_permission: bool,
) {
    if manage_team_permission && invite_user_permission {
        return;
    }

    let email = std::mem::take(&mut team.email);
    let invite_id = std::mem::take(&mut team.invite_id);
    team.sanitize();

    if manage_team_permission {
        team.email = email;
    }
    if invite_user_permission {
        team.invite_id = invite_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_store::SqlStore;
    use sqlx::postgres::PgPoolOptions;

    fn unreachable_app() -> App {
        // Same 250ms cap as `channel.rs`'s tests: sqlx's default acquire timeout is 30 seconds.
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        App::new(SqlStore::from_pool(pool))
    }

    fn team() -> Team {
        Team {
            email: "owner@example.com".to_owned(),
            invite_id: "ry9i4er48tt8bukijy7i3u5y9a".to_owned(),
            ..Default::default()
        }
    }

    /// The pairing, all four cells. The two mixed cells are the ones a crossed transcription
    /// gets wrong, and each field's fate differs between them, so a swap cannot pass.
    #[test]
    fn each_permission_restores_only_its_own_field() {
        let cases = [
            // (manage_team, invite_user, email survives, invite_id survives)
            (true, true, true, true),
            (true, false, true, false),
            (false, true, false, true),
            (false, false, false, false),
        ];
        for (manage, invite, email_survives, invite_id_survives) in cases {
            let mut t = team();
            apply_team_sanitize(&mut t, manage, invite);
            assert_eq!(
                !t.email.is_empty(),
                email_survives,
                "manage_team={manage}, invite_user={invite}: email"
            );
            assert_eq!(
                !t.invite_id.is_empty(),
                invite_id_survives,
                "manage_team={manage}, invite_user={invite}: invite_id"
            );
        }
    }

    /// Nothing else is touched: `Sanitize` clears exactly two fields, and the sanitiser must not
    /// grow side effects the wire would carry.
    #[test]
    fn sanitising_touches_only_email_and_invite_id() {
        let mut t = team();
        t.display_name = "Kept".to_owned();
        t.name = "kept".to_owned();
        let before = t.clone();

        apply_team_sanitize(&mut t, false, false);

        let mut expected = before;
        expected.email.clear();
        expected.invite_id.clear();
        assert_eq!(t, expected);
    }

    /// Any store failure is Go's one error id at 500; a user in no teams is not an error at all,
    /// so there is no 404 branch to reproduce.
    #[tokio::test]
    async fn a_broken_teams_lookup_is_gos_500() {
        let err = unreachable_app()
            .get_teams_for_user("uuuuuuuuuuuuuuuuuuuuuuuuuu")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.team.get_all.app_error");
        assert!(err.params.is_none(), "Go passes nil params");
    }

    /// `get_team`'s failure branch is the **`finding`** id; the 404 branch's `find` is asserted
    /// separately because the two differ by one gerund and a swap is invisible until i18n runs.
    #[tokio::test]
    async fn a_broken_team_lookup_is_a_500_with_the_finding_id() {
        let err = unreachable_app()
            .get_team("tttttttttttttttttttttttttt")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.team.get.finding.app_error");
        assert!(
            err.params.is_none(),
            "Go passes nil params in both branches"
        );
        assert_eq!(err.where_, "GetTeam");
    }

    /// With both counts failing (unreachable store), the **total**'s error id is the one
    /// reported — Go reads that goroutine's channel first, and the sequential port preserves
    /// the precedence. The id is the `member_count` one without `active_`.
    #[tokio::test]
    async fn a_broken_stats_lookup_reports_the_total_counts_error_first() {
        let err = unreachable_app()
            .get_team_stats("tttttttttttttttttttttttttt")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(
            err.id, "app.team.get_member_count.app_error",
            "the total count is read first, so its error wins when both fail"
        );
        assert_eq!(err.where_, "GetTeamStats");
        assert!(
            err.params.is_none(),
            "Go passes nil params in both branches"
        );
    }

    /// The active count's id differs from the total's only by an inserted `active_`; pinned as a
    /// literal because the unreachable-store test above can only ever see the total's.
    #[test]
    fn the_active_count_id_inserts_active_into_the_shared_id() {
        let active = mm_model::utils::AppError::new(
            "GetTeamStats",
            "app.team.get_active_member_count.app_error",
            None,
            String::new(),
            500,
        );
        assert_eq!(active.id, "app.team.get_active_member_count.app_error");
        assert_ne!(active.id, "app.team.get_member_count.app_error");
    }

    /// The 404's id is `find`, not `finding` — pinned as a literal because no fixture can reach
    /// the miss branch without a database (the parity suite covers it over REST).
    #[test]
    fn the_team_miss_id_is_find_not_finding() {
        let miss = mm_model::utils::AppError::new(
            "GetTeam",
            "app.team.get.find.app_error",
            None,
            String::new(),
            404,
        );
        assert_eq!(miss.id, "app.team.get.find.app_error", "team.go:903");
        assert_ne!(miss.id, "app.team.get.finding.app_error");
    }
}
