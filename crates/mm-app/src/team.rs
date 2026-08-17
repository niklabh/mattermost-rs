//! Port of the team app-layer surface (channels/app/team.go), `GetTeamMembersForUser` only.

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
}
