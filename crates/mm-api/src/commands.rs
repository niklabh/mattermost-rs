//! `getCommand` (api4/command.go:321) and the `custom_only` half of `listCommands` (:260).
//!
//! # `getCommand` answers 404 to every refusal, and that is the whole route
//!
//! Four different conditions — the command does not exist, the caller cannot view its team, the
//! caller cannot manage slash commands there, or the caller neither created it nor may manage
//! others' — all end in `SetCommandNotFoundError`. Go's comments say why twice: "here we return
//! Not_found instead of a permissions error so we don't leak the existence of a command to someone
//! without permissions for the team it belongs to". A 403 anywhere in that cascade tells a caller
//! that a command id is real and which team it belongs to.
//!
//! The error id is `store.sql_command.save.get.app_error` with `where` = `GetCommand`
//! (web/context.go:226) — a **store save** id on a read, produced by the *handler* rather than by
//! the store. It does not match the id `App.GetCommand` builds, and that one is unreachable
//! through this route.
//!
//! # `listCommands` is served for `custom_only` and forwarded otherwise
//!
//! The other branch calls `ListAutocompleteCommands`, which merges the **built-in** slash commands
//! — a registry of ~30 providers with translated display names, in `app/slashcommands/` — with
//! plugin-registered ones. Neither is ported, and neither can be synthesised from the database, so
//! that branch is forwarded. Everything before it is served: the missing `team_id`, the
//! `view_team` gate, and the `manage_own_slash_commands` gate that `custom_only` adds. See
//! [D-241].

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::command::Command;
use mm_model::permission::{
    PERMISSION_MANAGE_OTHERS_SLASH_COMMANDS, PERMISSION_MANAGE_OWN_SLASH_COMMANDS,
    PERMISSION_VIEW_TEAM, make_permission_error,
};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{query_first, query_flag_is_true};
use crate::error::ApiError;

/// `SetCommandNotFoundError` (web/context.go:226) — the one answer `getCommand` gives to a miss
/// and to all three of its refusals.
fn command_not_found() -> ApiError {
    ApiError::from(AppError::new(
        "GetCommand",
        "store.sql_command.save.get.app_error",
        None,
        String::new(),
        404,
    ))
}

/// `json.NewEncoder(w).Encode` — **both** routes here, so both bodies end in a newline.
fn encoded_ok<T: serde::Serialize>(value: &T, where_: &'static str) -> Result<Response, ApiError> {
    let mut body = serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise commands");
        ApiError::from(AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

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

/// Port of `getCommand` (command.go:321) — `GET /api/v4/commands/{command_id}`.
///
/// # The fetch precedes every permission question, and it has to
///
/// Two of the three gates are `SessionHasPermissionToTeam` on `cmd.TeamId`, which is not known
/// until the row is loaded. Because every refusal is the same 404, the ordering leaks nothing —
/// unlike `getJob` and `getUserAccessToken`, where the same shape *is* an existence oracle.
#[tracing::instrument(skip_all, fields(command_id = %command_id, team_id))]
pub async fn get_command(
    State(state): State<AppState>,
    Path(command_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireCommandId` (web/context.go:623) — the URL-param error, *not* the 404.
    if !is_valid_id(&command_id) {
        return Err(ApiError::invalid_url_param("command_id"));
    }

    // **Every** error from `GetCommand` becomes the 404, including the 501 the disabled setting
    // produces and the 500 a broken query would. That is Go's `if err != nil` (command.go:328).
    let Ok(command) = state.app.get_command(&command_id).await else {
        return Err(command_not_found());
    };
    tracing::Span::current().record("team_id", &command.team_id);

    if !state
        .app
        .session_has_permission_to_team(&session.0, &command.team_id, &PERMISSION_VIEW_TEAM)
        .await
    {
        return Err(command_not_found());
    }
    if !state
        .app
        .session_has_permission_to_team(
            &session.0,
            &command.team_id,
            &PERMISSION_MANAGE_OWN_SLASH_COMMANDS,
        )
        .await
    {
        return Err(command_not_found());
    }
    if session.0.user_id != command.creator_id
        && !state
            .app
            .session_has_permission_to_team(
                &session.0,
                &command.team_id,
                &PERMISSION_MANAGE_OTHERS_SLASH_COMMANDS,
            )
            .await
    {
        return Err(command_not_found());
    }

    encoded_ok(&command, "getCommand")
}

/// Port of `listCommands` (command.go:260) — `GET /api/v4/commands`.
///
/// Served for `custom_only=true`; forwarded otherwise. See the module note.
///
/// # `team_id` is a **body**-param error even though it is in the query string
///
/// `c.SetInvalidParam("team_id")` (command.go:265) gives `api.context.invalid_body_param.app_error`
/// for a missing query parameter — the id every other route reserves for a request *body*.
/// Reproduced, not tidied.
///
/// # `custom_only` narrows twice
///
/// It demands `manage_own_slash_commands` on the team, and then chooses the creator filter:
/// `manage_others_slash_commands` clears it, everyone else sees only their own. Getting the
/// polarity wrong shows every command in the team to a caller entitled to one.
#[tracing::instrument(skip_all, fields(team_id, custom_only, forwarded, count))]
pub async fn list_commands(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match list_commands_inner(&state, query.as_deref(), &session).await {
        Ok(Some(response)) => response,
        Ok(None) => {
            tracing::Span::current().record("forwarded", true);
            crate::proxy::forward_to_go(State(state), request).await
        }
        Err(err) => err.into_response(),
    }
}

/// `Ok(None)` means "this is the built-in-commands branch; forward".
async fn list_commands_inner(
    state: &AppState,
    query: Option<&str>,
    session: &AuthenticatedSession,
) -> Result<Option<Response>, ApiError> {
    // `strconv.ParseBool` with the error discarded — read **before** `team_id`, though nothing
    // observable depends on that order.
    let custom_only = query_flag_is_true(query, "custom_only");
    tracing::Span::current().record("custom_only", custom_only);

    let team_id = query_first(query, "team_id").unwrap_or_default();
    if team_id.is_empty() {
        return Err(ApiError::invalid_param("team_id"));
    }
    tracing::Span::current().record("team_id", &team_id);

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_VIEW_TEAM],
        )));
    }

    if !custom_only {
        // `ListAutocompleteCommands` / `ListAllCommandsByUser` — the built-in registry.
        return Ok(None);
    }

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_OWN_SLASH_COMMANDS)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_OWN_SLASH_COMMANDS],
        )));
    }

    // Empty means "every creator".
    let creator_filter = if state
        .app
        .session_has_permission_to_team(
            &session.0,
            &team_id,
            &PERMISSION_MANAGE_OTHERS_SLASH_COMMANDS,
        )
        .await
    {
        String::new()
    } else {
        session.0.user_id.clone()
    };

    let commands: Vec<Command> = state
        .app
        .list_team_commands_by_user(&team_id, &creator_filter)
        .await?;
    tracing::Span::current().record("count", commands.len());
    tracing::Span::current().record("forwarded", false);

    Ok(Some(encoded_ok(&commands, "listCommands")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal is a **404 with a `save` id**, produced by the handler and not by the store —
    /// and it is what all four of `getCommand`'s failure paths answer. A 403 anywhere would leak
    /// that a command id is real.
    #[test]
    fn every_refusal_is_the_same_not_found() {
        let err = command_not_found();
        assert_eq!(err.0.status_code, 404);
        assert_eq!(err.0.id, "store.sql_command.save.get.app_error");
        assert_eq!(err.0.where_, "GetCommand");
        // Not the id `App.GetCommand` builds — that one is unreachable through this route.
        assert_ne!(err.0.id, "store.sql_command.get.missing.app_error");
    }

    /// The missing `team_id` uses the **body**-param id for a query-string parameter. One word
    /// apart from the url-param one, and clients branch on it.
    #[test]
    fn the_missing_team_id_is_a_body_param_error() {
        let err = ApiError::invalid_param("team_id");
        assert_eq!(err.0.id, "api.context.invalid_body_param.app_error");
        assert_eq!(err.0.status_code, 400);
        assert_ne!(err.0.id, "api.context.invalid_url_param.app_error");
    }
}
