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
    pub(crate) async fn validate_command_trigger_uniqueness(
        &self,
        team_id: &str,
        trigger: &str,
        exclude_command_id: &str,
    ) -> AppResult {
        let trigger = trigger.to_lowercase();

        if built_in_command_triggers(&self.config(), self.export_file_backend().generates_links())
            .iter()
            .any(|built_in| built_in.to_lowercase() == trigger)
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
    pub async fn create_command(&self, command: Command) -> AppResult<Command> {
        if !self.config().enable_commands {
            return Err(commands_disabled("CreateCommand"));
        }
        self.create_command_ungated(command).await
    }

    /// Port of the unexported `App.createCommand` (command.go:717): [`App::create_command`]
    /// without the `EnableCommands` gate, which the plugin API's `CreateCommand` calls directly.
    pub async fn create_command_ungated(&self, mut command: Command) -> AppResult<Command> {
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

/// The triggers `validateCommandTriggerUniqueness` (command.go:741) reserves: every registered
/// provider whose `GetCommand` is non-nil, asked through
/// [`crate::command_provider::provider_command`] exactly as Go ranges over `commandProviders`.
///
/// Thirty-three providers are unconditional. Two return `nil` and so **free** their trigger for a
/// custom command: `/test` unless `ServiceSettings.EnableTesting`, and `/exportlink` unless
/// `FeatureFlags.EnableExportDirectDownload`, `FileSettings.DedicatedExportStore` and a
/// link-generating export backend all hold — `export_links`, see
/// [`crate::filestore::FileBackend::generates_links`].
fn built_in_command_triggers(config: &crate::config::Config, export_links: bool) -> Vec<String> {
    use crate::command_provider::{BUILTIN_TRIGGERS, ProviderCommand, provider_command};
    BUILTIN_TRIGGERS
        .iter()
        .filter_map(
            |trigger| match provider_command(config, export_links, trigger) {
                ProviderCommand::Command(command) => Some(command.trigger),
                ProviderCommand::Nil | ProviderCommand::Unregistered => None,
            },
        )
        .collect()
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

    /// An `App` whose export backend is built, as at boot, from `export_driver`; its store is a
    /// lazy pool that can never connect (250ms cap), so a check that reaches it is a 500.
    fn app_exporting_to(export_driver: &str) -> App {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        let mut config = crate::config::Config {
            dedicated_export_store: true,
            file_export_driver_name: export_driver.to_owned(),
            ..crate::config::Config::default()
        };
        config.feature_flags.enable_export_direct_download = true;
        App::with_config(mm_store::SqlStore::from_pool(pool), config)
    }

    /// D-260: with the flag and a dedicated store on, `/exportlink` is refused as a duplicate
    /// exactly when the export backend the app was built with generates links — the 400 before
    /// any team command is read — and otherwise passes the built-in check to the store.
    #[tokio::test]
    async fn exportlink_is_reserved_by_the_backend_the_app_was_built_with() {
        for driver in ["amazons3", "azureblob"] {
            let err = app_exporting_to(driver)
                .validate_command_trigger_uniqueness("team", "ExportLink", "")
                .await
                .expect_err("reserved");
            assert_eq!(
                err.id, "api.command.duplicate_trigger.app_error",
                "{driver}"
            );
            assert_eq!(err.status_code, 400);
        }
        let err = app_exporting_to("local")
            .validate_command_trigger_uniqueness("team", "exportlink", "")
            .await
            .expect_err("the unreachable store");
        assert_eq!(
            err.id, "app.command.validatecommandtriggeruniqueness.internal_error",
            "a local export backend frees the trigger, so the check goes on to the team's commands"
        );
    }

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
    /// because a provider that wrongly went nil would silently let a client register a custom
    /// command that shadows a built-in. `parity::command_writes` checks the *contents* against
    /// the running Go server.
    #[test]
    fn the_built_in_list_is_thirty_three_plus_the_two_conditional_ones() {
        let mut config = crate::config::Config::default();
        let stock = built_in_command_triggers(&config, true);
        assert_eq!(stock.len(), 33);
        assert!(!stock.iter().any(|t| t == "test"), "gated on EnableTesting");
        assert!(
            !stock.iter().any(|t| t == "exportlink"),
            "gated on EnableExportDirectDownload, off by default"
        );

        config.enable_testing = true;
        let testing = built_in_command_triggers(&config, false);
        assert_eq!(testing.len(), 34);
        assert!(testing.iter().any(|t| t == "test"));

        // D-260: reserved exactly when Go's `GetCommand` is non-nil.
        config.feature_flags.enable_export_direct_download = true;
        config.dedicated_export_store = true;
        assert_eq!(built_in_command_triggers(&config, false).len(), 34);
        let everything = built_in_command_triggers(&config, true);
        assert_eq!(everything.len(), 35);
        assert!(everything.iter().any(|t| t == "exportlink"));

        // `CmdCustomStatus` is `app.CmdCustomStatusTrigger`, whose *value* is `status`. Spelling
        // it `custom_status` would free the trigger `/status` for a custom command.
        assert!(stock.iter().any(|t| t == "status"));
        assert!(!stock.iter().any(|t| t == "custom_status"));

        let mut sorted = everything.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), everything.len(), "no duplicates");
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
