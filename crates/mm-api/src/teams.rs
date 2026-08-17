//! Port of `getTeamMembersForUser` (channels/api4/team.go:880), reached as
//! `GET /api/v4/users/me/teams/members`.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// Port of `getTeamMembersForUser` for the `me` case.
///
/// # Why this route is portable when its neighbours are not
///
/// Go guards a sanitiser with a permission check:
///
/// ```go
/// if !c.App.SessionHasPermissionToTeam(session, m.TeamId, model.PermissionManageTeamRoles) {
///     m.SanitizeRoleData(currentUserId)
/// }
/// ```
///
/// and `SessionHasPermissionToTeam` needs the roles-and-permissions system, which is unported.
/// But `SanitizeRoleData` is a **no-op when `o.UserId == currentUserId`** (team_member.go:147),
/// and this route returns the caller's *own* memberships — so every element satisfies that, and
/// the permission check's outcome cannot change the response.
///
/// The sanitiser is therefore called **unconditionally** here. For `me` that is provably
/// identical to Go; if the route were ever widened to `/users/{id}/teams/members` it would be
/// stricter than Go rather than looser, which is the safe direction to be wrong in. The two
/// preceding permission checks (`SessionHasPermissionToUser`, `UserCanSeeOtherUser`) both
/// short-circuit to `true` for self (authorization.go:258, user.go:2711), so they are true by
/// construction rather than skipped.
///
/// Contrast `GET /api/v4/users/me/teams`, which is **not** portable: its `SanitizeTeam` strips
/// `email` and `invite_id` based on two team-scoped permissions, and serving it without them
/// would leak an invite id. See [D-094].
///
/// # Wire format
///
/// `json.Marshal` + `w.Write` (team.go:914), so no trailing newline — same call-site rule as
/// `/users/me/sessions`, not `/users/me` ([D-086]).
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, count))]
pub async fn get_team_members_for_user_me(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `GetTeamMembersForUser(ctx, userId, "", true)` — no team excluded, deleted included. The
    // handler does not filter afterwards, so a deleted membership *is* returned here, unlike in
    // `SessionStore::Get` where Go drops them. Same store call, different post-processing.
    let mut members = state
        .app
        .get_team_members_for_user(&session.0.user_id, "", true)
        .await?;

    let current_user_id = &session.0.user_id;
    for member in &mut members {
        member.sanitize_role_data(current_user_id);
    }

    tracing::Span::current().record("count", members.len());

    let body = serde_json::to_vec(&members).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise team members");
        ApiError(mm_model::utils::AppError::new(
            "getTeamMembersForUser",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use mm_model::team_member::TeamMember;

    fn member(user_id: &str) -> TeamMember {
        TeamMember {
            team_id: "dpn4orkqniyzurpjzw6w6qxg8y".to_owned(),
            user_id: user_id.to_owned(),
            roles: "team_user team_admin".to_owned(),
            delete_at: 0,
            scheme_guest: false,
            scheme_user: true,
            scheme_admin: true,
            explicit_roles: String::new(),
            create_at: 0,
        }
    }

    const ME: &str = "y9i4er48tt8bukijy7i3u5y9ar";

    /// The claim this route's portability rests on: for the caller's own membership the sanitiser
    /// changes nothing, so the permission check Go wraps it in cannot affect the response.
    #[test]
    fn sanitize_role_data_is_a_no_op_for_ones_own_membership() {
        let mut mine = member(ME);
        let before = mine.clone();
        mine.sanitize_role_data(ME);
        assert_eq!(mine, before, "self-sanitising must not change anything");
    }

    /// And the other half, which is why calling it unconditionally is the safe direction: for
    /// someone else's membership it strips the role data and marks `delete_at` with Go's -1
    /// sentinel.
    #[test]
    fn sanitize_role_data_strips_another_users_membership() {
        let mut theirs = member("aaaaaaaaaaaaaaaaaaaaaaaaaa");
        theirs.sanitize_role_data(ME);

        assert_eq!(theirs.roles, "");
        assert_eq!(theirs.explicit_roles, "");
        assert!(!theirs.scheme_admin && !theirs.scheme_user && !theirs.scheme_guest);
        assert_eq!(theirs.delete_at, -1, "Go uses -1, not 0, as the sentinel");
    }

    /// `json.Marshal`, not an encoder — no trailing newline on this route.
    #[test]
    fn the_body_has_no_trailing_newline() {
        let body = serde_json::to_vec(&vec![member(ME)]).expect("serialises");
        assert_ne!(body.last(), Some(&b'\n'));
    }

    /// An empty membership list is `[]`, not `null`.
    #[test]
    fn an_empty_list_serialises_as_an_array() {
        let members: Vec<TeamMember> = Vec::new();
        assert_eq!(serde_json::to_string(&members).expect("serialises"), "[]");
    }
}
