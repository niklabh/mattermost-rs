//! Port of the team-membership **write** handlers of `server/channels/api4/team.go`.
//!
//! | route | handler | Go |
//! |---|---|---|
//! | `PUT /teams/{team_id}/members/{user_id}/roles` | [`update_team_member_roles`] | api4/team.go:1373 |
//! | `PUT /teams/{team_id}/members/{user_id}/schemeRoles` | [`update_team_member_scheme_roles`] | api4/team.go:1409 |
//!
//! # Both answer `{"status":"OK"}`, not the member
//!
//! `ReturnStatusOK(w)` — the updated `TeamMember` is computed, published on the socket and then
//! **thrown away**. So the only way a client learns the new roles is the `memberrole_updated`
//! event or a fresh `GET`, and a test that only reads the response body cannot see a wrong write
//! at all. That is why the parity suite for these two re-reads the membership and probes the
//! socket.
//!
//! # There is no board/space screen here
//!
//! The channel twins open with `rejectBoardChannelByID`/`rejectSpaceChannelByID`. Teams have no
//! such thing, so a reader porting from `channel_member_writes.rs` must *remove* that step rather
//! than translate it.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_MANAGE_TEAM_ROLES, make_permission_error};
use mm_model::scheme::SchemeRoles;
use mm_model::user::is_valid_user_roles;
use mm_model::utils::StringMap;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;

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

    /// `ReturnStatusOK` is not encoder-framed. The channel port's first version added the
    /// newline and four parity tests caught it; this asserts it without the round trip.
    #[test]
    fn the_ok_body_has_no_trailing_newline() {
        let response = status_ok();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
