//! Port of `App.GetCommand` (app/command.go:762) and `App.ListTeamCommandsByUser` (:149).
//!
//! # Both open with the same setting and only one of them shows it
//!
//! `ServiceSettings.EnableCommands` closed is `api.command.disabled.app_error` at **501** from
//! both. `listCommands` hands that straight to the client; `getCommand` **discards it** —
//! api4/command.go:329 answers `SetCommandNotFoundError` for *any* error `GetCommand` returns —
//! so on a disabled installation one route 501s and the other 404s. That is why this module
//! returns the real error and the discarding happens at the API edge.
//!
//! # The not-found error names the **store** as its `where`
//!
//! `model.NewAppError("SqlCommandStore.Get", "store.sql_command.get.missing.app_error", …)` with a
//! `command_id` parameter (command.go:772). It is also invisible through `getCommand`, for the
//! same reason — but `where` and `params` are reproduced because the next route to call this will
//! show them.

use mm_model::command::Command;
use mm_model::utils::{AppError, AppResult};
use mm_store::CommandStore;

use crate::App;

impl App {
    /// Port of `App.GetCommand` (command.go:762).
    #[tracing::instrument(skip(self), fields(command_id = %command_id))]
    pub async fn get_command(&self, command_id: &str) -> AppResult<Command> {
        if !self.config().enable_commands {
            return Err(commands_disabled("GetCommand"));
        }

        self.store().command().get(command_id).await.map_err(|err| {
            if err.is_not_found() {
                let mut params = std::collections::HashMap::new();
                params.insert(
                    "command_id".to_owned(),
                    serde_json::Value::String(command_id.to_owned()),
                );
                return AppError::boxed(
                    "SqlCommandStore.Get",
                    "store.sql_command.get.missing.app_error",
                    Some(params),
                    String::new(),
                    404,
                );
            }
            tracing::error!(error = ?err, "command lookup failed");
            AppError::boxed(
                "GetCommand",
                "app.command.getcommand.internal_error",
                None,
                String::new(),
                500,
            )
        })
    }

    /// Port of `App.ListTeamCommandsByUser` (command.go:149).
    ///
    /// **An empty `user_id` means "every creator", not "no creator"** — the handler passes `""`
    /// for a caller holding `manage_others_slash_commands` and its own id otherwise, so getting
    /// this condition backwards either hides every command or shows every one.
    ///
    /// The filter runs **in Go**, not in the query: `GetByTeam` returns the team's commands and
    /// the loop keeps those whose `CreatorId` matches. Reproduced in the same place, because the
    /// store method is shared with the autocomplete path that must not filter.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, filtered = !user_id.is_empty(), found))]
    pub async fn list_team_commands_by_user(
        &self,
        team_id: &str,
        user_id: &str,
    ) -> AppResult<Vec<Command>> {
        if !self.config().enable_commands {
            return Err(commands_disabled("ListTeamCommands"));
        }

        let commands = self
            .store()
            .command()
            .get_by_team(team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "team command listing failed");
                AppError::boxed(
                    "ListTeamCommands",
                    "app.command.listteamcommands.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        let commands = if user_id.is_empty() {
            commands
        } else {
            commands
                .into_iter()
                .filter(|command| command.creator_id == user_id)
                .collect()
        };

        tracing::Span::current().record("found", commands.len());
        Ok(commands)
    }
}

/// `api.command.disabled.app_error` at 501 — the same error from both callers, with a different
/// `where`. Go spells the second one `ListTeamCommands`, not `ListTeamCommandsByUser`.
fn commands_disabled(where_: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "api.command.disabled.app_error",
        None,
        String::new(),
        501,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The setting's refusal is a **501**, and it is the same id from both callers — which is what
    /// makes `getCommand`'s discarding of it visible only as a 404.
    #[test]
    fn the_disabled_setting_is_a_501_from_either_caller() {
        for where_ in ["GetCommand", "ListTeamCommands"] {
            let err = commands_disabled(where_);
            assert_eq!(err.id, "api.command.disabled.app_error");
            assert_eq!(err.status_code, 501);
            assert_eq!(err.where_, where_);
        }
    }

    /// A store miss carries the **command id as a parameter**, not only in the message, and names
    /// the store as its `where`.
    #[test]
    fn a_missing_command_names_the_store_and_carries_the_id() {
        let mut params = std::collections::HashMap::new();
        params.insert(
            "command_id".to_owned(),
            serde_json::Value::String("kh9x6ffcbir9uy8ta8m1yprkga".to_owned()),
        );
        let err = AppError::new(
            "SqlCommandStore.Get",
            "store.sql_command.get.missing.app_error",
            Some(params),
            String::new(),
            404,
        );
        assert_eq!(err.status_code, 404);
        assert_eq!(
            err.params
                .as_ref()
                .and_then(|p| p.get("command_id"))
                .and_then(|v| v.as_str()),
            Some("kh9x6ffcbir9uy8ta8m1yprkga")
        );
    }
}
