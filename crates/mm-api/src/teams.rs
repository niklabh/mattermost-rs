//! Ported handlers from `channels/api4/team.go`:
//!
//! - `getTeamsForUser` — `GET /api/v4/users/{user_id}/teams`
//! - `getTeamMembersForUser` — `GET /api/v4/users/me/teams/members` (`me` only)

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS, make_permission_error,
};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
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
/// Contrast [`get_teams_for_user`], which was *not* portable this way and stayed forwarded until
/// `SessionHasPermissionToTeam` landed: its `SanitizeTeam` strips `email` and `invite_id` based
/// on two team-scoped permissions with no self-shortcut. See [D-094] for the distinction.
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

/// Go's permission gate for [`get_teams_for_user`] (api4/team.go:737): asking about oneself is
/// always allowed; asking about anyone else needs `sysconsole_read_user_management_users` — a
/// **system-console** permission, not `edit_other_users` like `getChannelUnread`'s user gate.
/// Same shape either way: which permission a gate names is invisible over HTTP (the 403s are
/// identical with `detailed_error` wiped, [D-092]), so it is pinned here where a unit test can
/// hold it.
///
/// Note the self test is a plain string comparison against the session's user id — Go does
/// **not** call `SessionHasPermissionToUser` here, so its `manage_system`/unrestricted shortcuts
/// never run and the empty-target-denies rule does not apply. The permission check is the only
/// branch, and it is skipped entirely for self, which a test asserts by never polling it.
async fn teams_for_user_denied<F, Fut>(
    session_user_id: &str,
    target_user_id: &str,
    sysconsole_allowed: F,
) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    session_user_id != target_user_id && !sysconsole_allowed().await
}

/// Port of `getTeamsForUser` (api4/team.go:731), reached as
/// `GET /api/v4/users/{user_id}/teams`.
///
/// The route [D-094] classified as **not escapable**: `SanitizeTeam` strips `email` and
/// `invite_id` per two team-scoped permissions with no self-shortcut, so it stayed forwarded —
/// with a test keeping it forwarded — until `SessionHasPermissionToTeam` existed. That checker
/// landed with the permission system, so the route now serves from Rust and the keeping-it-
/// forwarded test asserts the opposite.
///
/// # Order of operations
///
/// 1. **`me` resolves before validation** (web/context.go:301), as everywhere.
/// 2. `RequireUserId`.
/// 3. **Self, or `sysconsole_read_user_management_users`** — see [`teams_for_user_denied`].
/// 4. `GetTeamsForUser`, then `SanitizeTeams` over every element. The sanitiser runs for
///    **self too**: being a team's plain member grants neither `manage_team` nor `invite_user`,
///    so one's own team list usually has both fields stripped — that is Go's answer, not
///    over-sanitising.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write` (team.go:750) — **no trailing newline**, same call-site rule as
/// `/users/me/teams/members` above and unlike the channel routes ([D-086]). An empty team list
/// is `[]`, never `null`, because Go's store initialises the slice.
#[tracing::instrument(skip_all, fields(user_id = %user_id, count))]
pub async fn get_teams_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireUserId` substitutes the session's id for `me` **before** the validity check
    // (web/context.go:301), so the alias works and an invalid literal still 400s.
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    require_id(&user_id, "user_id")?;

    let denied = teams_for_user_denied(&session.0.user_id, &user_id, || async {
        state
            .app
            .session_has_permission_to(
                &session.0,
                &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS,
            )
            .await
    })
    .await;

    if denied {
        return Err(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS],
        )));
    }

    let mut teams = state.app.get_teams_for_user(&user_id).await?;
    tracing::Span::current().record("count", teams.len());

    state.app.sanitize_teams(&session.0, &mut teams).await;

    let body = serde_json::to_vec(&teams).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the team list");
        ApiError(mm_model::utils::AppError::new(
            "getTeamsForUser",
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

    /// Asking about oneself is a string comparison, not a permission check — the gate closure
    /// must never be polled, or a self request would issue role queries Go does not.
    #[tokio::test]
    async fn asking_about_oneself_never_polls_the_sysconsole_gate() {
        let denied = super::teams_for_user_denied(ME, ME, || async {
            panic!("the gate must not run for self")
        })
        .await;
        assert!(!denied);
    }

    /// Anyone else needs the sysconsole permission, in both directions.
    #[tokio::test]
    async fn asking_about_another_user_takes_the_sysconsole_gate() {
        let other = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(!super::teams_for_user_denied(ME, other, || async { true }).await);
        assert!(super::teams_for_user_denied(ME, other, || async { false }).await);
    }

    /// The 403 names `sysconsole_read_user_management_users` — a system-console permission, not
    /// `edit_other_users` like the channel routes' user gate. The name is only in the wiped
    /// `detailed_error` ([D-092]), so it is pinned here.
    #[test]
    fn the_denial_names_the_sysconsole_permission() {
        let session = mm_model::session::Session {
            user_id: ME.to_owned(),
            ..Default::default()
        };
        let err = super::make_permission_error(
            &session,
            &[&super::PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_USERS],
        );
        assert_eq!(err.status_code, 403);
        assert_eq!(err.id, "api.context.permissions.app_error");
        assert_eq!(
            err.detailed_error,
            format!("userId={ME}, permission=sysconsole_read_user_management_users")
        );
    }

    /// An empty **team** list is `[]` too, and the body has no trailing newline — this route is
    /// `json.Marshal` + `w.Write`, not an encoder ([D-086]).
    #[test]
    fn the_team_list_body_is_an_array_with_no_newline() {
        let teams: Vec<mm_model::team::Team> = Vec::new();
        let body = serde_json::to_vec(&teams).expect("serialises");
        assert_eq!(body, b"[]");
        assert_ne!(body.last(), Some(&b'\n'));
    }
}
