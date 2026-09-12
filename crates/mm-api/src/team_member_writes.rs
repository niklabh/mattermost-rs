//! Port of the team-membership **write** handlers of `server/channels/api4/team.go`.
//!
//! | route | handler | Go |
//! |---|---|---|
//! | `PUT /teams/{team_id}/members/{user_id}/roles` | [`update_team_member_roles`] | api4/team.go:1373 |
//! | `PUT /teams/{team_id}/members/{user_id}/schemeRoles` | [`update_team_member_scheme_roles`] | api4/team.go:1409 |
//! | `POST /teams/{team_id}/members` | [`add_team_member`] | api4/team.go:976 |
//! | `POST /teams/{team_id}/members/batch` | [`add_team_members`] | api4/team.go:1131 |
//! | `DELETE /teams/{team_id}/members/{user_id}` | [`remove_team_member`] | api4/team.go:1271 |
//!
//! # What the add routes do not do
//!
//! - **A group-constrained team is forwarded to Go.** `FilterNonGroupTeamMembers` needs the group
//!   store's syncable-membership query, and Go's refusal names every denied user id in the error
//!   params — not a thing to guess at.
//! - **No join system post, and no `Users.UpdateAt` bump.** See
//!   [`mm_app::App::join_user_to_team`]; recorded as **D-242** and **D-243**.
//!
//! # The two role routes answer `{"status":"OK"}`, not the member
//!
//! `ReturnStatusOK(w)` — the updated `TeamMember` is computed, published on the socket and then
//! **thrown away**. So the only way a client learns the new roles is the `memberrole_updated`
//! event or a fresh `GET`, and a test that only reads the response body cannot see a wrong write
//! at all. That is why the parity suite for those two re-reads the membership and probes the
//! socket. The two add routes *do* answer with what they wrote — in three different framings
//! between them, see each handler.
//!
//! # There is no board/space screen here
//!
//! The channel twins open with `rejectBoardChannelByID`/`rejectSpaceChannelByID`. Teams have no
//! such thing, so a reader porting from `channel_member_writes.rs` must *remove* that step rather
//! than translate it.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_ADD_USER_TO_TEAM, PERMISSION_INVITE_GUEST, PERMISSION_JOIN_PRIVATE_TEAMS,
    PERMISSION_JOIN_PUBLIC_TEAMS, PERMISSION_MANAGE_TEAM_ROLES, PERMISSION_REMOVE_USER_FROM_TEAM,
    make_permission_error,
};
use mm_model::scheme::SchemeRoles;
use mm_model::team_member::{TeamMember, team_members_with_error_to_team_members};
use mm_model::user::is_valid_user_roles;
use mm_model::utils::{AppError, StringMap, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;

/// Port of `removeTeamMember` (api4/team.go:1271) — `DELETE /teams/{team_id}/members/{user_id}`.
///
/// # The permission gate is **skipped entirely** for a self-removal
///
/// `if session.UserId != params.UserId { … }`. So leaving a team needs no permission at all —
/// not `remove_user_from_team`, not team membership, not even that the team exists yet, since
/// the gate precedes both reads. Hoisting the check out of that `if` would refuse every ordinary
/// member trying to leave.
///
/// # Two reads, then one group check, then the cascade
///
/// `GetTeam` and `GetUser` both precede the group-constraint refusal, so a removal naming a
/// missing team is a 404 before it can be a 400. The refusal itself is three ANDed clauses:
/// the team is group-constrained, **the target is not the caller**, and the target is **not a
/// bot**. A member of a group-constrained team can therefore still leave it, and a bot can still
/// be removed from it — both easy to lose, and each is a route that stops working if lost.
///
/// # Wire format
///
/// `ReturnStatusOK` — `{"status":"OK"}` with no trailing newline.
#[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id, self_removal))]
pub async fn remove_team_member(
    State(state): State<AppState>,
    Path((team_id, user_id)): Path<(String, String)>,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }

    let self_removal = session.0.user_id == user_id;
    tracing::Span::current().record("self_removal", self_removal);

    if !self_removal
        && !state
            .app
            .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_REMOVE_USER_FROM_TEAM)
            .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_REMOVE_USER_FROM_TEAM],
        ))
        .into_response();
    }

    let team = match state.app.get_team(&team_id).await {
        Ok(team) => team,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let user = match state.app.get_user(&user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if team.is_group_constrained() && !self_removal && !user.is_bot {
        return ApiError::from(mm_model::utils::AppError::new(
            "removeTeamMember",
            "api.team.remove_member.group_constrained.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    match state
        .app
        .remove_user_from_team(&team_id, &user_id, &session.0.user_id)
        .await
    {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `model.MapFromJSON` (utils.go:507) — **every** failure is an empty map.
///
/// Go's `json.NewDecoder(...).Decode(&objmap)` result is discarded and a nil map replaced with an
/// allocated one, so a body that is not an object, an object with a non-string value, and an
/// empty body are all indistinguishable from `{}`. The one divergence is a *partial* decode:
/// `{"roles":"team_user","n":5}` fills Go's map before failing and so yields `{"roles":…}`, where
/// `serde_json` has no partial result and yields `{}`. Same divergence as
/// [`crate::channel_member_writes`]'s copy; kept local rather than shared so neither module's
/// tests constrain the other.
fn map_from_json(bytes: &[u8]) -> StringMap {
    serde_json::from_slice::<StringMap>(bytes).unwrap_or_default()
}

/// Port of `web.ReturnStatusOK` (web/web.go:127) — `w.Write([]byte(MapToJSON(m)))`, so **no
/// trailing newline**, unlike every `json.NewEncoder(w).Encode` body in the tree.
fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        r#"{"status":"OK"}"#,
    )
        .into_response()
}

/// Go's `c.RequireTeamId().RequireUserId()`, in that order — the team id's error wins when both
/// segments are malformed. `RequireUserId` substitutes the session's id for the literal `me`
/// *before* validating, which is why the caller resolves `me` first.
#[allow(clippy::result_large_err)]
fn require_team_and_user(team_id: &str, user_id: &str) -> Result<(), ApiError> {
    require_id(team_id, "team_id")?;
    require_id(user_id, "user_id")?;
    Ok(())
}

/// Port of `updateTeamMemberRoles` (api4/team.go:1373) —
/// `PUT /api/v4/teams/{team_id}/members/{user_id}/roles`.
///
/// # The invalid-parameter name is `team_member_roles`, not `roles`
///
/// `c.SetInvalidParam("team_member_roles")` — so the 400 body reads
/// `api.context.invalid_body_param.app_error` with `{"Name":"team_member_roles"}`, where the
/// channel twin says `roles`. A client matching on the parameter name sees a different string on
/// the two routes for the same mistake.
///
/// # An empty body is **not** refused here
///
/// `props["roles"]` on a map without the key is `""`, and `IsValidUserRoles("")` is `true` — its
/// `strings.Fields` loop never runs. So `{}` passes this gate, reaches the app layer, clears
/// every scheme flag and fails four layers down as
/// `api.team.update_team_member_roles.unset_user_scheme.app_error`. That is a 400 either way but
/// a different id, and it is the branch that proves the check order.
///
/// # The body check runs **before** the permission check
///
/// So a caller holding nothing who sends `{"roles":"!"}` gets a 400, not a 403. Go's order, and
/// the same order as the channel twin.
#[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id))]
pub async fn update_team_member_roles(
    State(state): State<AppState>,
    Path((team_id, user_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_team_and_user(&team_id, &user_id) {
        return err.into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("team_member_roles").into_response();
        }
    };
    let props = map_from_json(&bytes);
    let new_roles = props.get("roles").map(String::as_str).unwrap_or_default();

    if !is_valid_user_roles(new_roles) {
        return ApiError::invalid_param("team_member_roles").into_response();
    }

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_TEAM_ROLES)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_TEAM_ROLES],
        ))
        .into_response();
    }

    match state
        .app
        .update_team_member_roles(&team_id, &user_id, new_roles)
        .await
    {
        Ok(_) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `updateTeamMemberSchemeRoles` (api4/team.go:1409) —
/// `PUT /api/v4/teams/{team_id}/members/{user_id}/schemeRoles`.
///
/// # This one *does* 400 on a malformed body
///
/// `json.NewDecoder(r.Body).Decode(&schemeRoles)` with
/// `SetInvalidParamWithErr("scheme_roles", err)`, where its `/roles` sibling swallows every
/// decode failure into an empty map. So `not json` is a 400 here and a
/// `unset_user_scheme` 400 there — same status, different id.
///
/// `model.SchemeRoles` is three plain `bool`s, so `{}` decodes cleanly to three `false`s and the
/// app layer refuses it with `unset_user_scheme`.
///
/// # `schemeRoles` is camelCase
///
/// gorilla matches the segment literally and so does axum, so `/schemeroles` reaches neither
/// router's handler and is forwarded to Go — which answers its own 404.
#[tracing::instrument(skip_all, fields(team_id = %team_id, user_id = %user_id))]
pub async fn update_team_member_scheme_roles(
    State(state): State<AppState>,
    Path((team_id, user_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_team_and_user(&team_id, &user_id) {
        return err.into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("scheme_roles").into_response();
        }
    };
    let scheme_roles: SchemeRoles = match serde_json::from_slice(&bytes) {
        Ok(roles) => roles,
        Err(err) => {
            tracing::debug!(error = %err, "scheme_roles body did not decode");
            return ApiError::invalid_param("scheme_roles").into_response();
        }
    };

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_TEAM_ROLES)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_TEAM_ROLES],
        ))
        .into_response();
    }

    match state
        .app
        .update_team_member_scheme_roles(
            &team_id,
            &user_id,
            scheme_roles.scheme_guest,
            scheme_roles.scheme_user,
            scheme_roles.scheme_admin,
        )
        .await
    {
        Ok(_) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `MaxAddMembersBatch` (api4/team.go:22).
const MAX_ADD_MEMBERS_BATCH: usize = 256;

/// Port of `addTeamMember` (api4/team.go:976) — `POST /api/v4/teams/{team_id}/members`.
///
/// # The permission matrix turns on **who is being added**, not on what is being asked
///
/// Adding *yourself* is a join and is gated on the team's privacy:
/// `join_public_teams` when `AllowOpenInvite`, `join_private_teams` otherwise — and both are
/// **system**-scoped checks (`SessionHasPermissionTo`), not team-scoped, because a non-member has
/// no team roles to consult. Adding *anybody else* is `add_user_to_team` **on the team**, plus a
/// second read: a caller without `invite_guest` must fetch the target user and is refused if that
/// user is a guest.
///
/// Note `AllowOpenInvite` alone decides the privacy branch — not `team.Type`. The privacy-update
/// route moves the flag without syncing the type, so keying on the type would let a caller join a
/// team Go treats as private.
///
/// # Two `GetTeam`s
///
/// The self-join branch reads the team, and then the function reads it again for the
/// group-constrained check. Go's; reproduced because the second read is what the
/// group-constrained branch sees and a cached first read could be stale by then.
///
/// # The body's `team_id` must match the path's
///
/// …and it is an `invalid_body_param` error naming `team_id`, not the URL one — a different id
/// for what looks like the same mistake as a malformed path segment.
#[tracing::instrument(skip_all, fields(team_id = %team_id, forwarded))]
pub async fn add_team_member(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return invalid_add_body().into_response();
        }
    };

    let Some(member) = decode_team_member(&bytes) else {
        tracing::debug!("the team member body did not decode");
        return invalid_add_body().into_response();
    };

    if member.team_id != team_id {
        return ApiError::invalid_param("team_id").into_response();
    }
    if !is_valid_id(&member.user_id) {
        return ApiError::invalid_param("user_id").into_response();
    }

    if member.user_id == session.0.user_id {
        let team = match state.app.get_team(&member.team_id).await {
            Ok(team) => team,
            Err(err) => return ApiError::from(err).into_response(),
        };

        if team.allow_open_invite {
            if !state
                .app
                .session_has_permission_to(&session.0, &PERMISSION_JOIN_PUBLIC_TEAMS)
                .await
            {
                return ApiError::from(*make_permission_error(
                    &session.0,
                    &[&PERMISSION_JOIN_PUBLIC_TEAMS],
                ))
                .into_response();
            }
        } else if !state.app.team_access_controlled(&team.id)
            && !state
                .app
                .session_has_permission_to(&session.0, &PERMISSION_JOIN_PRIVATE_TEAMS)
                .await
        {
            return ApiError::from(*make_permission_error(
                &session.0,
                &[&PERMISSION_JOIN_PRIVATE_TEAMS],
            ))
            .into_response();
        }
    } else {
        if !state
            .app
            .session_has_permission_to_team(
                &session.0,
                &member.team_id,
                &PERMISSION_ADD_USER_TO_TEAM,
            )
            .await
        {
            return ApiError::from(*make_permission_error(
                &session.0,
                &[&PERMISSION_ADD_USER_TO_TEAM],
            ))
            .into_response();
        }

        if let Some(err) = guest_screen(&state, &session, &team_id, &member.user_id).await {
            return err.into_response();
        }
    }

    let team = match state.app.get_team(&member.team_id).await {
        Ok(team) => team,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if team.group_constrained.unwrap_or(false) {
        // `FilterNonGroupTeamMembers` needs the group store's syncable-membership query, which
        // this port does not have. Go's answer is a 400 naming every denied user id; forwarding
        // gives that answer rather than guessing at it.
        tracing::Span::current().record("forwarded", true);
        return forward(state, parts, bytes).await;
    }
    tracing::Span::current().record("forwarded", false);

    let mut created = match state
        .app
        .add_team_member(&member.team_id, &member.user_id)
        .await
    {
        Ok(created) => created,
        Err(err) => return ApiError::from(err).into_response(),
    };

    // `SanitizeRoleData` blanks the roles and sets `delete_at` to **-1** for anyone but the
    // caller, so a member added by an admin without `manage_team_roles` comes back with its
    // `delete_at` negative. That sentinel is on the wire.
    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_TEAM_ROLES)
        .await
    {
        created.sanitize_role_data(&session.0.user_id);
    }

    encoded(StatusCode::CREATED, &created, "addTeamMember")
}

/// Port of `addTeamMembers` (api4/team.go:1131) —
/// `POST /api/v4/teams/{team_id}/members/batch`.
///
/// # `?graceful=` changes the response *type*
///
/// Any non-empty value turns it on — `r.URL.Query().Get("graceful") != ""`, so `?graceful=0` is
/// **on**. On, the body is a list of `{user_id, member, error}`; off, it is a plain list of
/// members and the first failure is the whole request's error, with the users already added left
/// added. There is no transaction either way.
///
/// # The checks are in a different order from the single-add route
///
/// Here the team is read and the group-constrained branch taken **before** the permission check,
/// where `addTeamMember` checks the permission first. So on a group-constrained team a caller
/// holding nothing gets the group refusal, not a 403 — and on an ordinary team the same caller
/// gets the 403. Swapping them is a one-line change and a different answer for two real callers.
///
/// # The body is written, not encoded
///
/// `w.Write(js)` after `json.Marshal`, so **no trailing newline** — unlike `addTeamMember`'s
/// `json.NewEncoder(w).Encode`. Two sibling routes, two framings.
#[tracing::instrument(skip_all, fields(team_id = %team_id, asked, graceful, forwarded))]
pub async fn add_team_members(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let graceful = request
        .uri()
        .query()
        .map(|query| has_non_empty_query_value(query, "graceful"))
        .unwrap_or(false);
    tracing::Span::current().record("graceful", graceful);

    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("members").into_response();
        }
    };

    let Some(members) = decode_team_members(&bytes) else {
        tracing::debug!("the team member list did not decode");
        return ApiError::invalid_param("members").into_response();
    };
    tracing::Span::current().record("asked", members.len());

    // Both of these use the *message* as the parameter name — `SetInvalidParam("too many members
    // in batch")` — so the `Name` in the error body is a sentence, not a field.
    if members.len() > MAX_ADD_MEMBERS_BATCH {
        return ApiError::invalid_param("too many members in batch").into_response();
    }
    if members.is_empty() {
        return ApiError::invalid_param("no members in batch").into_response();
    }

    let team = match state.app.get_team(&team_id).await {
        Ok(team) => team,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if team.group_constrained.unwrap_or(false) {
        tracing::Span::current().record("forwarded", true);
        return forward(state, parts, bytes).await;
    }
    tracing::Span::current().record("forwarded", false);

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_ADD_USER_TO_TEAM)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_ADD_USER_TO_TEAM],
        ))
        .into_response();
    }

    let mut user_ids: Vec<String> = Vec::with_capacity(members.len());
    for member in &members {
        if member.team_id != team_id {
            return ApiError::invalid_param(&format!(
                "team_id for member with user_id={}",
                member.user_id
            ))
            .into_response();
        }
        if !is_valid_id(&member.user_id) {
            return ApiError::invalid_param("user_id").into_response();
        }
        if let Some(err) = guest_screen(&state, &session, &team_id, &member.user_id).await {
            return err.into_response();
        }
        user_ids.push(member.user_id.clone());
    }

    let mut results = match state
        .app
        .add_team_members(&team_id, &user_ids, &session.0.user_id, graceful)
        .await
    {
        Ok(results) => results,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_TEAM_ROLES)
        .await
    {
        for entry in &mut results {
            if let Some(member) = entry.member.as_mut() {
                member.sanitize_role_data(&session.0.user_id);
            }
        }
    }

    let body = if graceful {
        serde_json::to_vec(&results)
    } else {
        serde_json::to_vec(&team_members_with_error_to_team_members(&results))
    };
    let Ok(body) = body else {
        return ApiError::from(AppError::new(
            "addTeamMembers",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
        .into_response();
    };

    // `w.Write(js)` — **not** an encoder, so no trailing newline.
    (
        StatusCode::CREATED,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

/// Go's shared guest screen on both add routes: a caller **without** `invite_guest` on the team
/// has to read the target user, and a guest is refused.
///
/// Two things ride on it. A caller holding `invite_guest` never reads a single user — Go hoists
/// that permission read out of the batch loop and this port repeats it per member instead, which
/// costs a role lookup per member and cannot change the answer, since neither the session nor the
/// team moves inside the loop. And the user lookup's 404 id is
/// **`api.team.user.missing_account`**, with `addTeamMembers` as the `where` on *both* routes:
/// the single-add handler names its sibling there (api4/team.go:1037).
async fn guest_screen(
    state: &AppState,
    session: &AuthenticatedSession,
    team_id: &str,
    user_id: &str,
) -> Option<ApiError> {
    if state
        .app
        .session_has_permission_to_team(&session.0, team_id, &PERMISSION_INVITE_GUEST)
        .await
    {
        return None;
    }

    let user = match state.app.get_user(user_id).await {
        Ok(user) => user,
        Err(err) => {
            tracing::debug!(error = %err, "the target user could not be read");
            return Some(ApiError::from(AppError::new(
                "addTeamMembers",
                "api.team.user.missing_account",
                None,
                String::new(),
                404,
            )));
        }
    };

    if user.is_guest() {
        return Some(ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_INVITE_GUEST],
        )));
    }
    None
}

/// `r.URL.Query().Get(name) != ""` — Go's presence test for `graceful`.
///
/// Go's `Get` returns the **first** value for the key, so `?graceful=&graceful=1` is *off*. A
/// reader who reached for "any value is truthy" would get that backwards.
fn has_non_empty_query_value(query: &str, name: &str) -> bool {
    query
        .split('&')
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .find(|(key, _)| *key == name)
        .is_some_and(|(_, value)| !value.is_empty())
}

/// Go's `json.NewDecoder(r.Body).Decode(&member)` into a **struct**: a JSON object decodes, an
/// explicit `null` leaves the zero value and succeeds, and everything else is an error.
///
/// `serde`'s derived `Deserialize` is looser in exactly one direction that matters here: a struct
/// also deserializes **from a sequence**, and `#[serde(default)]` fills the missing tail — so
/// `from_slice::<TeamMember>(b"[]")` yields a zero-valued member where Go returns a decode error.
/// The two then answer *different* 400s, `api.context.invalid_body_param.app_error` against
/// `api.team.add_team_member.invalid_body.app_error`. Measured: the first version of this handler
/// did exactly that and the parity suite caught it.
fn decode_team_member(bytes: &[u8]) -> Option<TeamMember> {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        // `Decode` into a non-pointer struct leaves it untouched for `null`, and Go carries on
        // with the zero value — which then fails the `team_id` comparison below.
        Ok(serde_json::Value::Null) => Some(TeamMember::default()),
        Ok(value @ serde_json::Value::Object(_)) => serde_json::from_value(value).ok(),
        _ => None,
    }
}

/// The list form, with the same screen applied to the outer value and to every element.
///
/// Go decodes into `[]*TeamMember`, so a `null` *element* is a nil pointer that the handler's
/// first field access would dereference — a panic the mux recovers into a 500. This port cannot
/// reproduce a panic and treats it as a zero-valued member instead, which is a 400. The only way
/// to reach it is a body containing a literal `null` inside the array.
fn decode_team_members(bytes: &[u8]) -> Option<Vec<TeamMember>> {
    match serde_json::from_slice::<serde_json::Value>(bytes) {
        // A `null` body is a nil slice, which the length checks then call "no members in batch".
        Ok(serde_json::Value::Null) => Some(Vec::new()),
        Ok(serde_json::Value::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                serde_json::Value::Null => Some(TeamMember::default()),
                value @ serde_json::Value::Object(_) => serde_json::from_value(value).ok(),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// `model.NewAppError("addTeamMember", "api.team.add_team_member.invalid_body.app_error", nil,
/// "Error in model.TeamMemberFromJSON()", 400)` — a route-specific id rather than the shared
/// `invalid_body_param` one, and it carries a fixed detail string.
fn invalid_add_body() -> ApiError {
    ApiError::from(AppError::new(
        "addTeamMember",
        "api.team.add_team_member.invalid_body.app_error",
        None,
        "Error in model.TeamMemberFromJSON()".to_owned(),
        400,
    ))
}

/// `json.NewEncoder(w).Encode(v)` — with the trailing newline that framing leaves.
fn encoded<T: serde::Serialize>(status: StatusCode, value: &T, where_: &'static str) -> Response {
    let Ok(mut body) = serde_json::to_vec(value) else {
        return ApiError::from(AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
        .into_response();
    };
    body.push(b'\n');
    (
        status,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

async fn forward(
    state: AppState,
    parts: axum::http::request::Parts,
    bytes: axum::body::Bytes,
) -> Response {
    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    crate::proxy::forward_to_go(State(state), request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three inputs that make `/roles` and `/schemeRoles` disagree about the same body.
    #[test]
    fn map_from_json_swallows_everything_the_scheme_roles_decoder_refuses() {
        assert!(map_from_json(b"").is_empty());
        assert!(map_from_json(b"not json").is_empty());
        assert!(map_from_json(b"[1,2]").is_empty());
        assert!(map_from_json(br#"{"roles":5}"#).is_empty());
        assert_eq!(
            map_from_json(br#"{"roles":"team_user"}"#).get("roles"),
            Some(&"team_user".to_owned())
        );

        // The same three bodies against `SchemeRoles`, which is the route's 400.
        for body in [b"not json".as_slice(), b"[1,2]".as_slice()] {
            assert!(serde_json::from_slice::<SchemeRoles>(body).is_err());
        }
        let empty: SchemeRoles = serde_json::from_slice(b"{}").unwrap();
        assert!(!empty.scheme_guest && !empty.scheme_user && !empty.scheme_admin);
    }

    /// `props["roles"]` on a map with no `roles` key is `""`, and Go's `IsValidUserRoles("")` is
    /// **true** — the loop over `strings.Fields` never runs. The `/roles` route therefore does
    /// *not* 400 on an empty body; the refusal comes from the app layer instead.
    #[test]
    fn an_absent_roles_key_passes_the_handlers_gate() {
        let props = map_from_json(b"{}");
        let roles = props.get("roles").map(String::as_str).unwrap_or_default();
        assert_eq!(roles, "");
        assert!(is_valid_user_roles(roles));

        // And what it does refuse: a name that is not a valid role identifier.
        assert!(!is_valid_user_roles("!"));
        assert!(!is_valid_user_roles("team_user !"));
        assert!(is_valid_user_roles("team_user team_admin"));
    }

    /// Go chains `RequireTeamId().RequireUserId()`, so a request with two malformed segments
    /// reports the **team** id. Invisible over HTTP once `detailed_error` is wiped ([D-092]),
    /// which is exactly why it is pinned here.
    #[test]
    fn the_team_id_is_validated_first() {
        let err = require_team_and_user("bad", "also-bad").unwrap_err();
        assert!(
            format!("{err:?}").contains("team_id"),
            "the team id's refusal must win: {err:?}"
        );
        assert!(require_team_and_user("y9i4er48tt8bukijy7i3u5y9ar", "bad").is_err());
        assert!(
            require_team_and_user("y9i4er48tt8bukijy7i3u5y9ar", "y9i4er48tt8bukijy7i3u5y9ar")
                .is_ok()
        );
    }

    /// The struct-from-sequence trap. `serde` accepts `[]` for a struct and Go does not, and the
    /// two produce *different* 400s — so the screen is the difference between two error ids.
    #[test]
    fn only_an_object_or_null_decodes_as_a_team_member() {
        assert!(decode_team_member(b"[]").is_none(), "Go errors on a list");
        assert!(decode_team_member(b"[1,2]").is_none());
        assert!(decode_team_member(b"5").is_none());
        assert!(decode_team_member(b"\"x\"").is_none());
        assert!(decode_team_member(b"").is_none());
        assert!(decode_team_member(b"{").is_none());

        // `Decode` into a struct leaves the zero value for an explicit `null` and reports no
        // error, so the handler carries on and refuses it on the `team_id` comparison instead.
        let zero = decode_team_member(b"null").expect("null is not an error in Go");
        assert_eq!(zero.team_id, "");

        let member = decode_team_member(br#"{"team_id":"t","user_id":"u"}"#).expect("an object");
        assert_eq!(member.team_id, "t");
        assert_eq!(member.user_id, "u");

        // A field of the wrong type is an error on both sides.
        assert!(decode_team_member(br#"{"delete_at":"soon"}"#).is_none());
    }

    /// The list form applies the same screen at both levels.
    #[test]
    fn only_an_array_of_objects_decodes_as_a_batch() {
        assert!(
            decode_team_members(b"{}").is_none(),
            "Go errors on an object"
        );
        assert!(
            decode_team_members(b"[[]]").is_none(),
            "…and on a nested list"
        );
        assert!(decode_team_members(b"[5]").is_none());
        assert_eq!(
            decode_team_members(b"null").expect("a nil slice").len(),
            0,
            "a null body is an empty batch, which the length check then refuses"
        );
        assert_eq!(
            decode_team_members(br#"[{"team_id":"t","user_id":"u"}]"#)
                .expect("a list")
                .len(),
            1
        );
    }

    /// `r.URL.Query().Get(name) != ""` — presence with a **non-empty** value, and `Get` takes the
    /// first occurrence.
    #[test]
    fn graceful_is_any_non_empty_first_value() {
        assert!(has_non_empty_query_value("graceful=1", "graceful"));
        assert!(
            has_non_empty_query_value("graceful=0", "graceful"),
            "`0` is a non-empty string, so it turns it on"
        );
        assert!(has_non_empty_query_value("a=b&graceful=x&c=d", "graceful"));
        assert!(!has_non_empty_query_value("graceful=", "graceful"));
        assert!(
            !has_non_empty_query_value("graceful", "graceful"),
            "a bare key has the empty value"
        );
        assert!(!has_non_empty_query_value("other=1", "graceful"));
        assert!(
            !has_non_empty_query_value("graceful=&graceful=1", "graceful"),
            "Get returns the first value, not the first non-empty one"
        );
    }

    /// `MaxAddMembersBatch` is 256 and the comparison is `len(members) > MaxAddMembersBatch`, so
    /// a batch of exactly 256 is accepted and 257 is not.
    #[test]
    fn the_batch_cap_is_inclusive() {
        assert_eq!(MAX_ADD_MEMBERS_BATCH, 256);
        let refused = |len: usize| len > MAX_ADD_MEMBERS_BATCH;
        assert!(!refused(256));
        assert!(refused(257));
        assert!(!refused(0), "an empty batch is refused by the other check");
    }

    /// `ReturnStatusOK` is not encoder-framed. The channel port's first version added the
    /// newline and four parity tests caught it; this asserts it without the round trip.
    #[test]
    fn the_ok_body_has_no_trailing_newline() {
        let response = status_ok();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
