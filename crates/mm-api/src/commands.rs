//! The slash-command CRUD routes: `getCommand` (api4/command.go:321), the `custom_only` half of
//! `listCommands` (:260), and the five writes — `createCommand` (:31), `updateCommand` (:82),
//! `moveCommand` (:143), `deleteCommand` (:214) and `regenCommandToken` (:506).
//!
//! `executeCommand` and the two autocomplete routes are not here: they reach the built-in
//! provider registry and the plugin host.
//!
//! # The 404-for-a-403 rule covers the writes too, but not uniformly
//!
//! Every write repeats `getCommand`'s ownership ladder, and the **first** rung — no
//! `manage_own_slash_commands` on the command's team — answers 404 in all five, for the reason Go
//! comments twice. The **second** rung, "not the creator and no `manage_others`", answers a plain
//! **403** everywhere except `getCommand`. So `PUT /commands/{id}` distinguishes those two
//! refusals and `GET /commands/{id}` does not, on the same command and the same caller. That is
//! Go's, not an oversight in this port; [`command_not_found`] is used only where Go uses it.
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
use mm_model::command_request::CommandMoveRequest;
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

/// `json.NewEncoder(w).Encode` — every JSON body on this module's routes ends in a newline.
///
/// Via [`mm_model::utils::go_json_marshal`], not `serde_json::to_vec`: `encoding/json` escapes
/// `<`, `>` and `&` to `\u003c`, `\u003e` and `\u0026` in every string it writes, and serde does
/// not. A command's `url` is the field that makes that reachable — `?a=1&b=2` is an ordinary
/// callback URL and would otherwise differ from Go byte for byte.
fn encoded<T: serde::Serialize>(
    status: StatusCode,
    value: &T,
    where_: &'static str,
) -> Result<Response, ApiError> {
    let body = mm_model::utils::go_json_marshal(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise commands");
        ApiError::from(AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    Ok((
        status,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body + "\n",
    )
        .into_response())
}

/// The 200 case, which is every read plus `updateCommand`.
fn encoded_ok<T: serde::Serialize>(value: &T, where_: &'static str) -> Result<Response, ApiError> {
    encoded(StatusCode::OK, value, where_)
}

/// `ReturnStatusOK` (web/web.go:127) — `model.MapToJSON`, written with `w.Write`, so **no
/// trailing newline**. `moveCommand` and `deleteCommand` answer with exactly these bytes.
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

/// `c.SetPermissionError(permission)` — the plain 403, which the writes use for the *second*
/// ownership rung where `getCommand` uses a 404.
fn permission_error(
    session: &AuthenticatedSession,
    permission: &'static mm_model::permission::Permission,
) -> Response {
    ApiError::from(make_permission_error(&session.0, &[permission])).into_response()
}

/// `RequireCommandId` (web/context.go:623).
fn require_command_id(command_id: &str) -> Result<(), ApiError> {
    if !is_valid_id(command_id) {
        return Err(ApiError::invalid_url_param("command_id"));
    }
    Ok(())
}

/// The body every write but `moveCommand` reads: a whole `model.Command`.
///
/// Go is `json.NewDecoder(r.Body).Decode(&cmd)`, whose failure is
/// `SetInvalidParamWithErr("command")` — the body-param 400. `serde_json::from_slice` is stricter
/// than `Decode` about trailing bytes after the object; nothing a client sends reaches that.
async fn command_from_body(request: Request, parameter: &str) -> Result<Command, ApiError> {
    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return Err(ApiError::invalid_param(parameter));
        }
    };
    serde_json::from_slice(&bytes).map_err(|err| {
        tracing::debug!(error = %err, "command body did not decode");
        ApiError::invalid_param(parameter)
    })
}

/// The preamble `updateCommand`, `deleteCommand` and `regenCommandToken` share after the fetch,
/// and that `moveCommand` runs against the command's *current* team.
///
/// Two rungs, **two different answers**: no `manage_own_slash_commands` on the command's team is
/// the 404, and "neither the creator nor a holder of `manage_others_slash_commands`" is a 403.
/// Collapsing them would either leak a command's existence or hide a refusal a client must be
/// able to tell from a miss.
async fn refuse_unless_the_caller_may_manage(
    state: &AppState,
    session: &AuthenticatedSession,
    command: &Command,
) -> Option<Response> {
    if !state
        .app
        .session_has_permission_to_team(
            &session.0,
            &command.team_id,
            &PERMISSION_MANAGE_OWN_SLASH_COMMANDS,
        )
        .await
    {
        return Some(command_not_found().into_response());
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
        return Some(permission_error(
            session,
            &PERMISSION_MANAGE_OTHERS_SLASH_COMMANDS,
        ));
    }

    None
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

/// Port of `createCommand` (command.go:31) — `POST /api/v4/commands`.
///
/// # `team_id` comes from the body and is checked by a permission call, not by a validator
///
/// The first gate is `manage_own_slash_commands` **on the team the body names**. An empty
/// `team_id` denies at `SessionHasPermissionToTeam`'s own `teamID == ""` guard for an ordinary
/// caller — but a system admin's roles are consulted only *after* that guard, so the guard denies
/// them too, and an admin posting a command with no team gets the same 403 as anyone else. The
/// `model.command.is_valid.team_id.app_error` that shape suggests is unreachable through this
/// route.
///
/// # Naming someone else as the creator costs three things
///
/// `manage_others_slash_commands`, an existing user (`GetUser`'s own 404, not a 400), and then
/// the command is created **as that user**. Naming yourself, or leaving `creator_id` empty, skips
/// all three — so the cheap path and the expensive path differ by a string comparison.
///
/// `creator_id` is then overwritten unconditionally, which is why a body naming a `plugin_id`
/// cannot pass `IsValid`: the command ends up with both.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, team_id, trigger))]
pub async fn create_command(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let mut command = match command_from_body(request, "command").await {
        Ok(command) => command,
        Err(err) => return err.into_response(),
    };
    tracing::Span::current().record("team_id", &command.team_id);
    tracing::Span::current().record("trigger", &command.trigger);

    if !state
        .app
        .session_has_permission_to_team(
            &session.0,
            &command.team_id,
            &PERMISSION_MANAGE_OWN_SLASH_COMMANDS,
        )
        .await
    {
        return permission_error(&session, &PERMISSION_MANAGE_OWN_SLASH_COMMANDS);
    }

    let mut user_id = session.0.user_id.clone();
    if !command.creator_id.is_empty() && command.creator_id != user_id {
        if !state
            .app
            .session_has_permission_to_team(
                &session.0,
                &command.team_id,
                &PERMISSION_MANAGE_OTHERS_SLASH_COMMANDS,
            )
            .await
        {
            return permission_error(&session, &PERMISSION_MANAGE_OTHERS_SLASH_COMMANDS);
        }

        if let Err(err) = state.app.get_user(&command.creator_id).await {
            return ApiError::from(err).into_response();
        }

        user_id.clone_from(&command.creator_id);
    }

    command.creator_id = user_id;

    match state.app.create_command(command).await {
        Ok(created) => match encoded(StatusCode::CREATED, &created, "createCommand") {
            Ok(response) => response,
            Err(err) => err.into_response(),
        },
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `updateCommand` (command.go:82) — `PUT /api/v4/commands/{command_id}`.
///
/// # The body's `id` must equal the path's, and a mismatch is the *decode* error
///
/// `if jsonErr := …; jsonErr != nil || cmd.Id != c.Params.CommandId` — one condition, one answer.
/// So `PUT /commands/A` with `{"id":"B"}` is `api.context.invalid_body_param.app_error` with
/// `Name: command`, indistinguishable from malformed JSON, and **not** a 404 about `B`.
///
/// # `team_id` must be present and must match
///
/// Unlike `updateIncomingHook`, which fills an empty `team_id` from the old hook, this compares
/// the body's value to the old command's directly: omitting the field is
/// `api.command.team_mismatch.app_error` at 400, not a no-op. The check is belt and braces —
/// `App.UpdateCommand` copies `TeamId` off the old command anyway — but it is observable, so it
/// stays.
#[tracing::instrument(skip_all, fields(command_id = %command_id, user_id = %session.0.user_id))]
pub async fn update_command(
    State(state): State<AppState>,
    Path(command_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_command_id(&command_id) {
        return err.into_response();
    }

    let command = match command_from_body(request, "command").await {
        Ok(command) => command,
        Err(err) => return err.into_response(),
    };
    if command.id != command_id {
        return ApiError::invalid_param("command").into_response();
    }

    let Ok(old_command) = state.app.get_command(&command_id).await else {
        return command_not_found().into_response();
    };

    if command.team_id != old_command.team_id {
        return ApiError::from(AppError::new(
            "updateCommand",
            "api.command.team_mismatch.app_error",
            None,
            format!("user_id={}", session.0.user_id),
            400,
        ))
        .into_response();
    }

    if let Some(refusal) = refuse_unless_the_caller_may_manage(&state, &session, &old_command).await
    {
        return refusal;
    }

    match state.app.update_command(&old_command, command).await {
        Ok(updated) => match encoded_ok(&updated, "updateCommand") {
            Ok(response) => response,
            Err(err) => err.into_response(),
        },
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `deleteCommand` (command.go:214) — `DELETE /api/v4/commands/{command_id}`.
///
/// Fetch, the two ownership rungs, then a soft delete and `{"status":"OK"}`. Deleting an
/// already-deleted command is the 404 from the fetch, because the store's `DeleteAt = 0`
/// predicate hides it.
#[tracing::instrument(skip_all, fields(command_id = %command_id, user_id = %session.0.user_id))]
pub async fn delete_command(
    State(state): State<AppState>,
    Path(command_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_command_id(&command_id) {
        return err.into_response();
    }

    let Ok(command) = state.app.get_command(&command_id).await else {
        return command_not_found().into_response();
    };

    if let Some(refusal) = refuse_unless_the_caller_may_manage(&state, &session, &command).await {
        return refusal;
    }

    match state.app.delete_command(&command.id).await {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `moveCommand` (command.go:143) — `PUT /api/v4/commands/{command_id}/move`.
///
/// # Five gates, and the destination is checked before the command exists
///
/// 1. `GetTeam(body.team_id)` — a missing destination is that route's **404**, answered before
///    anything about `{command_id}` is known. So a bad team id and a bad command id give
///    different 404s, and the team one wins.
/// 2. `manage_own_slash_commands` on the **destination** team — a plain 403, and the only gate in
///    this family that is a 403 without a preceding creator check.
/// 3. `GetCommand` — the 404.
/// 4. The usual two rungs on the command's **current** team: 404, then 403.
/// 5. `HasPermissionToTeam(command.creator_id, destination, manage_own_slash_commands)` — the
///    **creator's** access, not the caller's, and a 400 rather than a 403 when it fails. Moving a
///    command into a team its owner cannot reach is what this forbids, and it is the one check
///    here that consults a user who is not making the request.
#[tracing::instrument(skip_all, fields(command_id = %command_id, team_id, user_id = %session.0.user_id))]
pub async fn move_command(
    State(state): State<AppState>,
    Path(command_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_command_id(&command_id) {
        return err.into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("team_id").into_response();
        }
    };
    // `SetInvalidParamWithErr("team_id")` — the *field*, not the type, names this one.
    let move_request: CommandMoveRequest = match serde_json::from_slice(&bytes) {
        Ok(move_request) => move_request,
        Err(err) => {
            tracing::debug!(error = %err, "move request body did not decode");
            return ApiError::invalid_param("team_id").into_response();
        }
    };
    tracing::Span::current().record("team_id", &move_request.team_id);

    let new_team = match state.app.get_team(&move_request.team_id).await {
        Ok(team) => team,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if !state
        .app
        .session_has_permission_to_team(
            &session.0,
            &new_team.id,
            &PERMISSION_MANAGE_OWN_SLASH_COMMANDS,
        )
        .await
    {
        return permission_error(&session, &PERMISSION_MANAGE_OWN_SLASH_COMMANDS);
    }

    let Ok(command) = state.app.get_command(&command_id).await else {
        return command_not_found().into_response();
    };

    if let Some(refusal) = refuse_unless_the_caller_may_manage(&state, &session, &command).await {
        return refusal;
    }

    if !state
        .app
        .has_permission_to_team(
            &command.creator_id,
            &new_team.id,
            &PERMISSION_MANAGE_OWN_SLASH_COMMANDS,
        )
        .await
    {
        return ApiError::from(AppError::new(
            "moveCommand",
            "api.command.move_command.creator_no_permission.app_error",
            None,
            format!("creator_id={} team_id={}", command.creator_id, new_team.id),
            400,
        ))
        .into_response();
    }

    match state.app.move_command(&new_team.id, command).await {
        Ok(_moved) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `regenCommandToken` (command.go:506) —
/// `PUT /api/v4/commands/{command_id}/regen_token`.
///
/// # The body is a bare map, and it is the only one in this module without a trailing newline
///
/// `w.Write([]byte(model.MapToJSON(resp)))` rather than `json.NewEncoder(w).Encode` — so the
/// answer is `{"token":"…"}` with nothing after it, where every other route here ends in `\n`.
/// The whole command is **not** returned; a client that wants the new `update_at` must re-read.
#[tracing::instrument(skip_all, fields(command_id = %command_id, user_id = %session.0.user_id))]
pub async fn regen_command_token(
    State(state): State<AppState>,
    Path(command_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_command_id(&command_id) {
        return err.into_response();
    }

    let Ok(command) = state.app.get_command(&command_id).await else {
        return command_not_found().into_response();
    };

    if let Some(refusal) = refuse_unless_the_caller_may_manage(&state, &session, &command).await {
        return refusal;
    }

    let regenerated = match state.app.regen_command_token(command).await {
        Ok(regenerated) => regenerated,
        Err(err) => return ApiError::from(err).into_response(),
    };

    // `model.MapToJSON(map[string]string{"token": …})` — a single-key map, so there is no key
    // order to reproduce, and the token is base32 so the HTML escaping cannot bite.
    let body = format!(
        r#"{{"token":{}}}"#,
        serde_json::Value::String(regenerated.token)
    );
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
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
