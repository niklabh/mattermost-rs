//! Port of `SqlCommandStore` (channels/store/sqlstore/command_store.go) — `Get`, `GetByTeam`,
//! `Save`, `Update` and `Delete`.
//!
//! Ported for `getCommand` (`GET /api/v4/commands/{command_id}`), the `custom_only` branch of
//! `listCommands` (`GET /api/v4/commands`), and the five slash-command writes: `createCommand`,
//! `updateCommand`, `deleteCommand`, `moveCommand` and `regenCommandToken`. `GetByTrigger`
//! belongs to command *execution*, which is not ported.
//!
//! # The validation lives **here**, not in the app layer
//!
//! Unlike the webhook store, whose app layer calls `PreSave`/`IsValid` before handing a struct
//! down, `SqlCommandStore.Save` calls `command.PreSave()` and `command.IsValid()` itself, and
//! `Update` calls `cmd.UpdateAt = model.GetMillis()` and `cmd.IsValid()` itself. That placement
//! is observable: `App.MoveCommand` and `App.RegenCommandToken` never touch `UpdateAt`, so the
//! only reason a move or a token regeneration bumps it is that the *store* does. Moving the
//! assignment up a layer would leave those two routes writing a stale `UpdateAt`.
//!
//! # `DeleteAt = 0` is part of both `WHERE` clauses, not a convention
//!
//! Go's delete on a command is a **soft** delete, and neither read offers an
//! `include_deleted` — so a deleted command is simply gone from both queries. That is why `Get`
//! answers `ErrNotFound` for an id that still has a row: the predicate, not the absence, is what
//! makes it missing.
//!
//! # `trigger` is a reserved word
//!
//! Go quotes it (`"\"trigger\""`, command_store.go:131) and so must this: unquoted, Postgres reads
//! `TRIGGER` as the DDL keyword and the statement fails to parse. Only `GetByTrigger` needs it in a
//! predicate, but the projection selects the column on every query.

use mm_model::command::Command;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.CommandStore` the two read routes need.
pub trait CommandStore {
    /// Port of `SqlCommandStore.Get` (command_store.go:95). `ErrNotFound` on a miss **or** on a
    /// soft-deleted row.
    fn get(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<Command, StoreError>> + Send;

    /// Port of `SqlCommandStore.GetByTeam` (command_store.go:112).
    ///
    /// **No `ORDER BY`** — Go has none, so the order is Postgres's. Go's initialiser is
    /// `commands := []*model.Command{}`, so an empty result is `[]` and never `null`.
    fn get_by_team(
        &self,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<Command>, StoreError>> + Send;

    /// Port of `SqlCommandStore.Save` (command_store.go:62).
    ///
    /// `&mut` because Go mutates the caller's struct and the caller then marshals it: `PreSave`
    /// mints the id and the token, and the API answers with those values. Three refusals, and
    /// the app layer maps them to **different statuses**:
    ///
    /// - a non-empty `Id` is [`StoreError::InvalidInput`] — Go's `NewErrInvalidInput("Command",
    ///   "CommandId", …)`, which `App.createCommand`'s `errors.As(nErr, &appErr)` does *not*
    ///   match, so it becomes a **500**, not a 400;
    /// - `IsValid` is [`StoreError::Invalid`], carrying the `AppError` through to the client
    ///   unchanged (a 400);
    /// - the insert itself is [`StoreError::Db`] (a 500).
    fn save(
        &self,
        command: &mut Command,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlCommandStore.Update` (command_store.go:124).
    ///
    /// **`DeleteAt` is neither set nor in the `WHERE`.** The `SET` list holds sixteen columns and
    /// `DeleteAt` is not one of them, and the predicate is `Id` alone — so an update cannot
    /// resurrect or bury a command, and a soft-deleted row is still updatable. Every caller
    /// reaches this through `App.GetCommand`, whose `DeleteAt = 0` predicate makes that
    /// unreachable through the REST API.
    ///
    /// `&mut` for the same reason as [`CommandStore::save`], and because this is where `UpdateAt`
    /// is assigned.
    fn update(
        &self,
        command: &mut Command,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlCommandStore.Delete` (command_store.go:110) — a **soft** delete.
    ///
    /// `DeleteAt` and `UpdateAt` both take `time`, so the two stamps on a deleted command are
    /// always equal.
    ///
    /// # Go cannot fail this, and we can
    ///
    /// Go's body is `if err != nil { errors.Wrapf(err, …) }` — the wrapped error is **discarded**,
    /// not returned, so `SqlCommandStore.Delete` returns `nil` unconditionally and the
    /// `app.command.deletecommand.internal_error` branch above it is dead code. This port returns
    /// the driver error instead of dropping it, which means a broken database answers
    /// `DELETE /api/v4/commands/{id}` with a 500 here and `{"status":"OK"}` in Go. Divergent only
    /// on a path no request can reach without the database already being down.
    fn delete(
        &self,
        command_id: &str,
        time: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlCommandStore {
    pool: PgPool,
}

impl SqlCommandStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of `commandsQuery`. Every column but `Id` is nullable while Go scans into plain
/// fields, so the `COALESCE`s give the zero value Go's struct would hold.
struct CommandRow {
    id: String,
    token: String,
    createat: i64,
    updateat: i64,
    deleteat: i64,
    creatorid: String,
    teamid: String,
    trigger: String,
    method: String,
    username: String,
    iconurl: String,
    autocomplete: bool,
    autocompletedesc: String,
    autocompletehint: String,
    displayname: String,
    description: String,
    url: String,
    pluginid: String,
}

impl From<CommandRow> for Command {
    fn from(row: CommandRow) -> Self {
        Command {
            id: row.id,
            token: row.token,
            create_at: row.createat,
            update_at: row.updateat,
            delete_at: row.deleteat,
            creator_id: row.creatorid,
            team_id: row.teamid,
            trigger: row.trigger,
            method: row.method,
            username: row.username,
            icon_url: row.iconurl,
            auto_complete: row.autocomplete,
            auto_complete_desc: row.autocompletedesc,
            auto_complete_hint: row.autocompletehint,
            display_name: row.displayname,
            description: row.description,
            url: row.url,
            plugin_id: row.pluginid,
            // Both are `db:"-"` in Go — computed by the autocomplete machinery, never stored.
            autocomplete_data: None,
            autocomplete_icon_data: String::new(),
        }
    }
}

impl CommandStore for SqlCommandStore {
    #[tracing::instrument(skip_all, fields(command_id = %id))]
    async fn get(&self, id: &str) -> Result<Command, StoreError> {
        let row = sqlx::query_as!(
            CommandRow,
            r#"
            SELECT id                              AS "id!",
                   COALESCE(token, '')             AS "token!",
                   COALESCE(createat, 0)           AS "createat!",
                   COALESCE(updateat, 0)           AS "updateat!",
                   COALESCE(deleteat, 0)           AS "deleteat!",
                   COALESCE(creatorid, '')         AS "creatorid!",
                   COALESCE(teamid, '')            AS "teamid!",
                   COALESCE("trigger", '')         AS "trigger!",
                   COALESCE(method, '')            AS "method!",
                   COALESCE(username, '')          AS "username!",
                   COALESCE(iconurl, '')           AS "iconurl!",
                   COALESCE(autocomplete, false)   AS "autocomplete!",
                   COALESCE(autocompletedesc, '')  AS "autocompletedesc!",
                   COALESCE(autocompletehint, '')  AS "autocompletehint!",
                   COALESCE(displayname, '')       AS "displayname!",
                   COALESCE(description, '')       AS "description!",
                   COALESCE(url, '')               AS "url!",
                   COALESCE(pluginid, '')          AS "pluginid!"
              FROM commands
             WHERE id = $1 AND deleteat = 0
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("selectone: command_id={id}"),
            source,
        })?;

        row.map(Command::from).ok_or_else(|| StoreError::NotFound {
            entity: "Command",
            criteria: format!("id={id}"),
        })
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, found))]
    async fn get_by_team(&self, team_id: &str) -> Result<Vec<Command>, StoreError> {
        let rows = sqlx::query_as!(
            CommandRow,
            r#"
            SELECT id                              AS "id!",
                   COALESCE(token, '')             AS "token!",
                   COALESCE(createat, 0)           AS "createat!",
                   COALESCE(updateat, 0)           AS "updateat!",
                   COALESCE(deleteat, 0)           AS "deleteat!",
                   COALESCE(creatorid, '')         AS "creatorid!",
                   COALESCE(teamid, '')            AS "teamid!",
                   COALESCE("trigger", '')         AS "trigger!",
                   COALESCE(method, '')            AS "method!",
                   COALESCE(username, '')          AS "username!",
                   COALESCE(iconurl, '')           AS "iconurl!",
                   COALESCE(autocomplete, false)   AS "autocomplete!",
                   COALESCE(autocompletedesc, '')  AS "autocompletedesc!",
                   COALESCE(autocompletehint, '')  AS "autocompletehint!",
                   COALESCE(displayname, '')       AS "displayname!",
                   COALESCE(description, '')       AS "description!",
                   COALESCE(url, '')               AS "url!",
                   COALESCE(pluginid, '')          AS "pluginid!"
              FROM commands
             WHERE teamid = $1 AND deleteat = 0
            "#,
            team_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("select: team_id={team_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(Command::from).collect())
    }

    #[tracing::instrument(skip_all, fields(command_id, trigger = %command.trigger))]
    async fn save(&self, command: &mut Command) -> Result<(), StoreError> {
        // **The id check comes before `PreSave`**, so a body carrying an id is refused rather
        // than having its id honoured — and `PreSave`'s `if o.Id == ""` guard is therefore
        // unreachable from this path.
        if !command.id.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "Command",
                field: "CommandId",
                value: command.id.clone(),
            });
        }

        command.pre_save();
        command
            .is_valid()
            .map_err(|app_error| StoreError::Invalid {
                entity: "Command",
                app_error,
            })?;
        tracing::Span::current().record("command_id", &command.id);

        sqlx::query!(
            r#"
            INSERT INTO commands
                (id, token, createat, updateat, deleteat, creatorid, teamid, "trigger", method,
                 username, iconurl, autocomplete, autocompletedesc, autocompletehint, displayname,
                 description, url, pluginid)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                    $18)
            "#,
            command.id,
            command.token,
            command.create_at,
            command.update_at,
            // Taken from the request body and never overwritten: `PreSave` does not touch it, so
            // `POST /commands` with a non-zero `delete_at` inserts a command that both read
            // queries immediately hide. Reproduced, because Go's 201 says nothing about it.
            command.delete_at,
            command.creator_id,
            command.team_id,
            command.trigger,
            command.method,
            command.username,
            command.icon_url,
            command.auto_complete,
            command.auto_complete_desc,
            command.auto_complete_hint,
            command.display_name,
            command.description,
            command.url,
            command.plugin_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("insert: command_id={}", command.id),
            source,
        })?;

        Ok(())
    }

    #[tracing::instrument(skip_all, fields(command_id = %command.id))]
    async fn update(&self, command: &mut Command) -> Result<(), StoreError> {
        // Go's first statement, and the reason a move or a token regeneration bumps `UpdateAt`
        // without either app function mentioning it.
        command.pre_update();
        command
            .is_valid()
            .map_err(|app_error| StoreError::Invalid {
                entity: "Command",
                app_error,
            })?;

        sqlx::query!(
            r#"
            UPDATE commands
               SET token = $2, createat = $3, updateat = $4, creatorid = $5, teamid = $6,
                   method = $7, username = $8, iconurl = $9, autocomplete = $10,
                   autocompletedesc = $11, autocompletehint = $12, displayname = $13,
                   description = $14, url = $15, pluginid = $16, "trigger" = $17
             WHERE id = $1
            "#,
            command.id,
            command.token,
            command.create_at,
            command.update_at,
            command.creator_id,
            command.team_id,
            command.method,
            command.username,
            command.icon_url,
            command.auto_complete,
            command.auto_complete_desc,
            command.auto_complete_hint,
            command.display_name,
            command.description,
            command.url,
            command.plugin_id,
            command.trigger,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update commands: command_id={}", command.id),
            source,
        })?;

        // Go checks `RowsAffected() > 1` and errors; the predicate is the primary key, so it
        // cannot. **Zero rows is not an error on either side** — updating an id that does not
        // exist answers 200 with the struct the caller sent, and no `store.ErrNotFound` is ever
        // produced here. That is why `App.UpdateCommand`'s not-found branch is unreachable.
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(command_id = %command_id))]
    async fn delete(&self, command_id: &str, time: i64) -> Result<(), StoreError> {
        sqlx::query!(
            r#"UPDATE commands SET deleteat = $1, updateat = $1 WHERE id = $2"#,
            time,
            command_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("delete: command_id={command_id}"),
            source,
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two `db:"-"` fields are **not** read from the row — they are computed by machinery this
    /// server does not have. A mapping that invented values for them would put keys on the wire
    /// that Go's stored command never carries.
    #[test]
    fn the_computed_fields_stay_unset() {
        let command: Command = CommandRow {
            id: "kh9x6ffcbir9uy8ta8m1yprkga".to_owned(),
            token: "cqjc7ec6bpy65jjamstkhpe6fr".to_owned(),
            createat: 1788600000001,
            updateat: 1788600000002,
            deleteat: 0,
            creatorid: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            teamid: "tnewcuy4ztgw5doi7j5ytqxg9w".to_owned(),
            trigger: "mmrs".to_owned(),
            method: "P".to_owned(),
            username: "".to_owned(),
            iconurl: "".to_owned(),
            autocomplete: true,
            autocompletedesc: "a description".to_owned(),
            autocompletehint: "[hint]".to_owned(),
            displayname: "MMRS".to_owned(),
            description: "".to_owned(),
            url: "https://example.invalid/hook".to_owned(),
            pluginid: "".to_owned(),
        }
        .into();

        assert_eq!(command.trigger, "mmrs", "the reserved word is selected");
        assert_eq!(command.autocomplete_data, None);
        assert_eq!(command.autocomplete_icon_data, "");
        assert!(command.auto_complete);
    }

    /// A pool that opens no connection until a query runs — both tests below return before one
    /// does. The `acquire_timeout` is capped because sqlx's default is **30 seconds** and an
    /// accidental round trip would cost that per test.
    fn unreachable_store() -> SqlCommandStore {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://unreachable:unreachable@127.0.0.1:1/none")
            .expect("a lazy pool needs no server");
        SqlCommandStore::new(pool)
    }

    fn a_valid_command() -> Command {
        Command {
            creator_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            team_id: "tnewcuy4ztgw5doi7j5ytqxg9w".to_owned(),
            trigger: "mmrs".to_owned(),
            method: "P".to_owned(),
            url: "https://example.invalid/hook".to_owned(),
            ..Command::default()
        }
    }

    /// **An id in the body is refused before `PreSave` runs**, and as `InvalidInput` rather than
    /// as a validation error — which is what makes `POST /api/v4/commands` with an `id` a **500**
    /// and not a 400: `App.createCommand` matches only `*model.AppError` and falls through to its
    /// internal-error branch for this one.
    #[tokio::test]
    async fn an_id_in_the_body_is_refused_before_presave() {
        let store = unreachable_store();
        let mut command = a_valid_command();
        command.id = "kh9x6ffcbir9uy8ta8m1yprkga".to_owned();

        let err = store
            .save(&mut command)
            .await
            .expect_err("a command carrying an id is refused");
        assert!(
            err.is_invalid_input(),
            "`NewErrInvalidInput`, not a validation AppError: {err:?}"
        );
        assert!(
            !matches!(err, StoreError::Invalid { .. }),
            "an `Invalid` here would reach the client as a 400 instead of a 500"
        );
        assert_eq!(
            command.token, "",
            "`PreSave` must not have run — it would have minted a token"
        );
        assert_eq!(command.create_at, 0, "nor a create_at");
    }

    /// `IsValid` runs **after** `PreSave`, so a command with no url is rejected with an id and a
    /// token already minted — and the error is the model's own `AppError`, carried through to the
    /// client unchanged.
    #[tokio::test]
    async fn presave_runs_before_isvalid_and_the_validation_error_is_carried() {
        let store = unreachable_store();
        let mut command = a_valid_command();
        command.url = String::new();

        let err = store
            .save(&mut command)
            .await
            .expect_err("a command with no url is refused");
        let StoreError::Invalid { app_error, .. } = err else {
            panic!("a validation failure must be `StoreError::Invalid`");
        };
        assert_eq!(app_error.id, "model.command.is_valid.url.app_error");
        assert_eq!(app_error.status_code, 400);

        assert_eq!(command.id.len(), 26, "`PreSave` ran first and minted an id");
        assert_eq!(command.token.len(), 26, "and a token");
        assert!(command.create_at > 0, "and a create_at");
    }

    /// `Update` assigns `UpdateAt` itself, before validating — the whole reason `MoveCommand` and
    /// `RegenCommandToken`, which never mention the field, still bump it.
    #[tokio::test]
    async fn update_assigns_update_at_before_validating() {
        let store = unreachable_store();
        let mut command = a_valid_command();
        command.id = "kh9x6ffcbir9uy8ta8m1yprkga".to_owned();
        command.token = "cqjc7ec6bpy65jjamstkhpe6fr".to_owned();
        command.create_at = 1_788_600_000_001;
        command.update_at = 1;
        // Fails validation, so the call returns before touching the unreachable pool.
        command.method = "X".to_owned();

        let err = store
            .update(&mut command)
            .await
            .expect_err("an unknown method is refused");
        let StoreError::Invalid { app_error, .. } = err else {
            panic!("a validation failure must be `StoreError::Invalid`");
        };
        assert_eq!(app_error.id, "model.command.is_valid.method.app_error");
        assert!(
            command.update_at > 1_788_600_000_001,
            "UpdateAt was assigned before IsValid ran, not after it passed: {}",
            command.update_at
        );
    }
}
