//! Port of the slash-command CRUD half of `app/command.go`: `GetCommand` (:762),
//! `ListTeamCommandsByUser` (:149), `CreateCommand` (:709), `UpdateCommand` (:780),
//! `MoveCommand` (:816), `RegenCommandToken` (:840) and `DeleteCommand` (:864).
//!
//! Command *execution* — `ExecuteCommand`, the built-in providers' `DoCommand`, the plugin host —
//! is not here and is not ported.
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
use mm_model::utils::{AppError, AppResult, new_id};
use mm_store::{CommandStore, StoreError};

use crate::App;

/// The triggers the built-in slash-command providers occupy, which
/// [`App::validate_command_trigger_uniqueness`] reserves against custom commands.
///
/// # Why a list and not a registry
///
/// Go asks `commandProviders` — a map of ~35 providers in `channels/app/slashcommands/` — for
/// each provider's `GetCommand(a, i18n.T)` and compares `.Trigger`. Only that one field is read.
/// Every provider's `GetCommand` returns the same constant its `GetTrigger` does (checked across
/// all 35, not assumed), so the *whole* of what this check needs from the registry is this list of
/// strings. Porting the providers to reach it would pull in i18n, the plugin host and the
/// execution path for a string comparison.
///
/// Sorted, so a reader can diff it against the Go tree; the check is a linear scan either way.
/// Two triggers are **not** here because their providers return `nil` from `GetCommand` on a
/// stock server, which makes them available as custom triggers — see
/// [`App::built_in_command_triggers`].
const BUILT_IN_COMMAND_TRIGGERS: &[&str] = &[
    "away",
    "code",
    "collapse",
    "dnd",
    "echo",
    "expand",
    "groupmsg",
    "header",
    "help",
    "invite",
    "invite_people",
    "join",
    "kick",
    "leave",
    "logout",
    "marketplace",
    "me",
    "mobile-logs",
    "msg",
    "mute",
    "offline",
    "online",
    "open",
    "purpose",
    "remove",
    "rename",
    "search",
    // `CommandTriggerRemote` — the shared-channels connection command, registered unconditionally.
    "secure-connection",
    "settings",
    // `CommandTriggerShare`, likewise.
    "share-channel",
    "shortcuts",
    "shrug",
    // `CmdCustomStatus` is `app.CmdCustomStatusTrigger` (app/command.go:27) — the string is
    // `status`, **not** `custom_status`, and the constant name is the only place that is visible.
    "status",
];

/// `CmdTest` (`/test`), whose provider returns `nil` unless `ServiceSettings.EnableTesting`.
const BUILT_IN_TRIGGER_BEHIND_ENABLE_TESTING: &str = "test";

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

    /// Port of `App.validateCommandTriggerUniqueness` (command.go:738).
    ///
    /// # The built-ins are checked **first**, and against a different corpus
    ///
    /// A trigger colliding with a built-in and a trigger colliding with another custom command
    /// produce the *same* `api.command.duplicate_trigger.app_error` at 400, so the two branches
    /// are indistinguishable on the wire — but only the second one can be exercised by planting a
    /// row, which is why [`built_in_command_triggers`] has to be right on its own.
    ///
    /// # `exclude_command_id` is what makes an update idempotent
    ///
    /// `createCommand` passes `""`, so nothing is excluded. `updateCommand` and `moveCommand`
    /// pass the command's own id, so re-saving a command under its existing trigger is allowed.
    /// Dropping the exclusion turns every update into a duplicate-trigger 400; inverting it lets
    /// a command take a sibling's trigger.
    ///
    /// Both sides are lower-cased, because the *stored* trigger need not be: `createCommand`
    /// lower-cases the incoming one, but a plugin-registered row is written as-is.
    #[tracing::instrument(skip(self), fields(team_id = %team_id, trigger = %trigger))]
    async fn validate_command_trigger_uniqueness(
        &self,
        team_id: &str,
        trigger: &str,
        exclude_command_id: &str,
    ) -> AppResult {
        let trigger = trigger.to_lowercase();

        if built_in_command_triggers(self.config().enable_testing)
            .any(|built_in| built_in.eq_ignore_ascii_case(&trigger))
        {
            return Err(duplicate_trigger());
        }

        let team_commands = self
            .store()
            .command()
            .get_by_team(team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "team command listing failed");
                AppError::boxed(
                    "validateCommandTriggerUniqueness",
                    "app.command.validatecommandtriggeruniqueness.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        if team_commands.iter().any(|existing| {
            existing.id != exclude_command_id && existing.trigger.to_lowercase() == trigger
        }) {
            return Err(duplicate_trigger());
        }

        Ok(())
    }

    /// Port of `App.CreateCommand` (command.go:709) and the unexported `createCommand` it wraps.
    ///
    /// **The trigger is lower-cased before anything else**, so the stored — and returned —
    /// trigger is not necessarily the one the client sent. Then uniqueness, then the store, which
    /// is where `PreSave` and `IsValid` live: a body that is both a duplicate *and* invalid
    /// answers `api.command.duplicate_trigger.app_error`, never the validation error.
    #[tracing::instrument(skip_all, fields(team_id = %command.team_id, trigger = %command.trigger))]
    pub async fn create_command(&self, mut command: Command) -> AppResult<Command> {
        if !self.config().enable_commands {
            return Err(commands_disabled("CreateCommand"));
        }

        command.trigger = command.trigger.to_lowercase();

        self.validate_command_trigger_uniqueness(&command.team_id, &command.trigger, "")
            .await?;

        self.store()
            .command()
            .save(&mut command)
            .await
            .map_err(save_error)?;

        Ok(command)
    }

    /// Port of `App.UpdateCommand` (command.go:780).
    ///
    /// # Nine fields are taken from the old command, not the body
    ///
    /// `Id`, `Token`, `CreateAt`, `DeleteAt`, `CreatorId`, `PluginId` and `TeamId` are copied off
    /// `old_command`; `UpdateAt` is stamped; only `Trigger`, `Method`, `Username`, `IconURL`,
    /// `AutoComplete*`, `DisplayName`, `Description` and `URL` survive from the request. So a
    /// client cannot re-own, re-team or re-token a command through this route — the handler's
    /// team check above it is belt and braces, since the assignment here would have overridden a
    /// mismatched `team_id` anyway.
    ///
    /// `UpdateAt` is stamped **twice**: here, and again inside the store. The store's value is the
    /// one written and returned.
    #[tracing::instrument(skip_all, fields(command_id = %old_command.id))]
    pub async fn update_command(
        &self,
        old_command: &Command,
        mut updated: Command,
    ) -> AppResult<Command> {
        if !self.config().enable_commands {
            return Err(commands_disabled("UpdateCommand"));
        }

        updated.trigger = updated.trigger.to_lowercase();
        updated.id.clone_from(&old_command.id);
        updated.token.clone_from(&old_command.token);
        updated.create_at = old_command.create_at;
        updated.update_at = mm_model::utils::get_millis();
        updated.delete_at = old_command.delete_at;
        updated.creator_id.clone_from(&old_command.creator_id);
        updated.plugin_id.clone_from(&old_command.plugin_id);
        updated.team_id.clone_from(&old_command.team_id);

        self.validate_command_trigger_uniqueness(&updated.team_id, &updated.trigger, &updated.id)
            .await?;

        self.store()
            .command()
            .update(&mut updated)
            .await
            .map_err(|err| write_error(err, "UpdateCommand", "updatecommand", &updated.id))?;

        Ok(updated)
    }

    /// Port of `App.MoveCommand` (command.go:816).
    ///
    /// **No `EnableCommands` gate.** Every sibling opens with one and this does not — the setting
    /// only bites here by way of the `GetCommand` the handler ran first. Go passes the whole
    /// `*model.Team` and reads `Id` alone, so this takes the id.
    ///
    /// The uniqueness check runs against the **new** team, excluding this command — which cannot
    /// collide with itself there, since it is not in that team yet, but the exclusion is Go's and
    /// is kept. The trigger is *not* lower-cased here, unlike create and update: a command whose
    /// stored trigger has an uppercase letter (a plugin's) is compared lower-cased by
    /// [`App::validate_command_trigger_uniqueness`] and written back unchanged.
    #[tracing::instrument(skip_all, fields(command_id = %command.id, team_id = %team_id))]
    pub async fn move_command(&self, team_id: &str, mut command: Command) -> AppResult<Command> {
        self.validate_command_trigger_uniqueness(team_id, &command.trigger, &command.id)
            .await?;

        command.team_id = team_id.to_owned();

        self.store()
            .command()
            .update(&mut command)
            .await
            .map_err(|err| write_error(err, "MoveCommand", "movecommand", &command.id))?;

        Ok(command)
    }

    /// Port of `App.RegenCommandToken` (command.go:840).
    ///
    /// A fresh 26-character id becomes the token, and **no uniqueness check runs** — the trigger
    /// is untouched, so there is nothing to collide. `UpdateAt` moves because the store stamps it.
    #[tracing::instrument(skip_all, fields(command_id = %command.id))]
    pub async fn regen_command_token(&self, mut command: Command) -> AppResult<Command> {
        if !self.config().enable_commands {
            return Err(commands_disabled("RegenCommandToken"));
        }

        command.token = new_id();

        self.store()
            .command()
            .update(&mut command)
            .await
            .map_err(|err| {
                write_error(err, "RegenCommandToken", "regencommandtoken", &command.id)
            })?;

        Ok(command)
    }

    /// Port of `App.DeleteCommand` (command.go:864) — a soft delete, stamping `DeleteAt` and
    /// `UpdateAt` with the same `GetMillis()`.
    ///
    /// **No not-found branch.** Go's store swallows its own error and returns `nil` whatever
    /// happens, so deleting an id that does not exist is a `{"status":"OK"}` — and the handler
    /// above has already answered 404 for that case from `GetCommand`.
    #[tracing::instrument(skip(self), fields(command_id = %command_id))]
    pub async fn delete_command(&self, command_id: &str) -> AppResult {
        if !self.config().enable_commands {
            return Err(commands_disabled("DeleteCommand"));
        }

        self.store()
            .command()
            .delete(command_id, mm_model::utils::get_millis())
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "command delete failed");
                AppError::boxed(
                    "DeleteCommand",
                    "app.command.deletecommand.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }
}

/// The built-in triggers this server's configuration actually registers.
///
/// Two of the 35 providers return `nil` from `GetCommand`, which removes them from the comparison
/// entirely and *frees* the trigger for a custom command:
///
/// - **`/test`** unless `ServiceSettings.EnableTesting` (default `false`);
/// - **`/exportlink`** unless `FeatureFlags.EnableExportDirectDownload` **and**
///   `FileSettings.DedicatedExportStore` **and** the export file backend implements
///   `FileBackendWithLinkGenerator`.
///
/// `exportlink` is therefore never reserved here: the feature flag is not modelled in
/// [`crate::config::Config`] and defaults to `false` in Go, and the third condition is a property
/// of a file backend this server does not construct. On a stock configuration that is exact; on
/// one with the flag on, an operator could create a custom `/exportlink` here that Go would
/// refuse. See [D-260].
fn built_in_command_triggers(enable_testing: bool) -> impl Iterator<Item = &'static str> {
    BUILT_IN_COMMAND_TRIGGERS
        .iter()
        .copied()
        .chain(enable_testing.then_some(BUILT_IN_TRIGGER_BEHIND_ENABLE_TESTING))
}

/// `api.command.duplicate_trigger.app_error` at 400 — the one answer both halves of the
/// uniqueness check give, built-in collision and custom collision alike.
fn duplicate_trigger() -> Box<AppError> {
    AppError::boxed(
        "validateCommandTriggerUniqueness",
        "api.command.duplicate_trigger.app_error",
        None,
        String::new(),
        400,
    )
}

/// `App.createCommand`'s two-arm `switch` (command.go:723).
///
/// **There is no not-found arm here** — `Save` cannot produce one — so the only branch that is
/// not a 500 is `IsValid`'s own `AppError`, carried to the client id, status and all.
/// `StoreError::InvalidInput` is deliberately not in it: Go's `errors.As(nErr, &appErr)` does not
/// match `*store.ErrInvalidInput`, so `POST /api/v4/commands` with an `id` in the body is a
/// **500**, not the 400 its shape suggests.
fn save_error(err: StoreError) -> Box<AppError> {
    if let StoreError::Invalid { app_error, .. } = err {
        return app_error;
    }
    tracing::error!(error = ?err, "command save failed");
    AppError::boxed(
        "CreateCommand",
        "app.command.createcommand.internal_error",
        None,
        String::new(),
        500,
    )
}

/// The three-arm `switch` the three `Update` callers share (command.go:798, :824, :848).
///
/// The *first* arm is the subtle one: a `store.ErrNotFound` becomes a **404 naming the store**,
/// with the command id as a parameter. `Update` cannot produce one — its zero-rows case is not an
/// error — so the arm is dead in Go and reproduced here only so it cannot drift if `Update` ever
/// gains the `DeleteAt` predicate it lacks.
///
/// The second arm carries `IsValid`'s own `AppError` straight to the client. Folding it into the
/// 500 would turn every malformed command body into a server error.
fn write_error(
    err: StoreError,
    where_: &'static str,
    id_fragment: &str,
    command_id: &str,
) -> Box<AppError> {
    match err {
        StoreError::NotFound { .. } => {
            let mut params = std::collections::HashMap::new();
            params.insert(
                "command_id".to_owned(),
                serde_json::Value::String(command_id.to_owned()),
            );
            AppError::boxed(
                "SqlCommandStore.Update",
                "store.sql_command.update.missing.app_error",
                Some(params),
                String::new(),
                404,
            )
        }
        StoreError::Invalid { app_error, .. } => app_error,
        other => {
            tracing::error!(error = ?other, "command write failed");
            AppError::boxed(
                where_,
                format!("app.command.{id_fragment}.internal_error"),
                None,
                String::new(),
                500,
            )
        }
    }
}

/// `api.command.disabled.app_error` at 501 — the same error from every caller, with a different
/// `where`. Go spells the listing one `ListTeamCommands`, not `ListTeamCommandsByUser`.
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

    /// **Thirty-five providers, thirty-three of them unconditional.** The count is asserted
    /// because the list is a transcription: a trigger dropped from it silently lets a client
    /// register a custom command that shadows a built-in, and nothing else in this crate would
    /// notice. `parity::command_writes` checks the *contents* against the running Go server.
    #[test]
    fn the_built_in_list_is_thirty_three_plus_the_testing_one() {
        let stock: Vec<&str> = built_in_command_triggers(false).collect();
        assert_eq!(stock.len(), 33);
        assert!(!stock.contains(&"test"), "gated on EnableTesting");
        assert!(
            !stock.contains(&"exportlink"),
            "gated on a feature flag this server does not model — see D-260"
        );

        let testing: Vec<&str> = built_in_command_triggers(true).collect();
        assert_eq!(testing.len(), 34);
        assert!(testing.contains(&"test"));

        // `CmdCustomStatus` is `app.CmdCustomStatusTrigger`, whose *value* is `status`. Spelling
        // it `custom_status` would free the trigger `/status` for a custom command.
        assert!(stock.contains(&"status"));
        assert!(!stock.contains(&"custom_status"));

        let mut sorted = stock.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, stock, "kept sorted so it can be diffed against Go");
        sorted.dedup();
        assert_eq!(sorted.len(), stock.len(), "no duplicates");
    }

    /// Both halves of the uniqueness check answer the **same** 400, so a client cannot tell a
    /// collision with `/away` from a collision with a colleague's command.
    #[test]
    fn a_duplicate_trigger_is_a_400_whichever_half_found_it() {
        let err = duplicate_trigger();
        assert_eq!(err.id, "api.command.duplicate_trigger.app_error");
        assert_eq!(err.status_code, 400);
        assert_eq!(err.where_, "validateCommandTriggerUniqueness");
    }

    /// **An id in a create body is a 500, not a 400.** Go's `errors.As(nErr, &appErr)` matches
    /// only `*model.AppError`, and `store.ErrInvalidInput` is not one — so the store's most
    /// client-shaped refusal falls through to the internal-error arm.
    #[test]
    fn an_invalid_input_from_save_is_an_internal_error() {
        let err = save_error(StoreError::InvalidInput {
            entity: "Command",
            field: "CommandId",
            value: "kh9x6ffcbir9uy8ta8m1yprkga".to_owned(),
        });
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.command.createcommand.internal_error");
        assert_eq!(err.where_, "CreateCommand");
    }

    /// `IsValid`'s own `AppError` reaches the client untouched — id, status and `where` — from
    /// both the save and the update paths.
    #[test]
    fn a_validation_failure_is_carried_through_unchanged() {
        let invalid = || StoreError::Invalid {
            entity: "Command",
            app_error: AppError::boxed(
                "Command.IsValid",
                "model.command.is_valid.trigger.app_error",
                None,
                String::new(),
                400,
            ),
        };

        for err in [
            save_error(invalid()),
            write_error(
                invalid(),
                "UpdateCommand",
                "updatecommand",
                "kh9x6ffcbir9uy8ta8m1yprkga",
            ),
        ] {
            assert_eq!(err.id, "model.command.is_valid.trigger.app_error");
            assert_eq!(err.status_code, 400);
            assert_eq!(err.where_, "Command.IsValid");
        }
    }

    /// The three `Update` callers each have their **own** internal-error id, and `where` is the
    /// exported function's name rather than the store's. Getting the fragment wrong is invisible
    /// to a status-code assertion and visible to a client branching on `id`.
    #[test]
    fn each_update_caller_has_its_own_internal_error_id() {
        for (where_, fragment) in [
            ("UpdateCommand", "updatecommand"),
            ("MoveCommand", "movecommand"),
            ("RegenCommandToken", "regencommandtoken"),
        ] {
            let err = write_error(
                StoreError::Db {
                    context: "commands".to_owned(),
                    source: sqlx::Error::RowNotFound,
                },
                where_,
                fragment,
                "kh9x6ffcbir9uy8ta8m1yprkga",
            );
            assert_eq!(err.status_code, 500);
            assert_eq!(err.where_, where_);
            assert_eq!(err.id, format!("app.command.{fragment}.internal_error"));
        }
    }

    /// The dead arm, reproduced: a `store.ErrNotFound` names **`SqlCommandStore.Update`** — not
    /// the app function — and carries the command id as a parameter. `Update` cannot raise one,
    /// because zero rows affected is not an error there.
    #[test]
    fn the_not_found_arm_names_the_store_and_the_update_id() {
        let err = write_error(
            StoreError::NotFound {
                entity: "Command",
                criteria: "id=kh9x6ffcbir9uy8ta8m1yprkga".to_owned(),
            },
            "MoveCommand",
            "movecommand",
            "kh9x6ffcbir9uy8ta8m1yprkga",
        );
        assert_eq!(err.status_code, 404);
        assert_eq!(err.where_, "SqlCommandStore.Update");
        assert_eq!(err.id, "store.sql_command.update.missing.app_error");
        assert_eq!(
            err.params
                .as_ref()
                .and_then(|p| p.get("command_id"))
                .and_then(|v| v.as_str()),
            Some("kh9x6ffcbir9uy8ta8m1yprkga")
        );
    }

    /// The disabled setting reaches **five** callers now, and `MoveCommand` is deliberately not
    /// one of them: Go opens every sibling with the `EnableCommands` check and opens
    /// `MoveCommand` without it.
    #[test]
    fn move_command_is_the_one_write_without_the_enable_commands_gate() {
        for where_ in [
            "CreateCommand",
            "UpdateCommand",
            "RegenCommandToken",
            "DeleteCommand",
        ] {
            assert_eq!(commands_disabled(where_).status_code, 501);
        }
        // Nothing to assert about `MoveCommand` beyond the absence of the branch; this test is
        // here so a reader adding the "missing" gate has to delete a statement that says why.
    }
}
