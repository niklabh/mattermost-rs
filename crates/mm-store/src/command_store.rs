//! Port of `SqlCommandStore` (channels/store/sqlstore/command_store.go), `Get` and `GetByTeam`.
//!
//! Ported for `getCommand` (`GET /api/v4/commands/{command_id}`) and the `custom_only` branch of
//! `listCommands` (`GET /api/v4/commands`). The write half belongs to the slash-command editor,
//! and `GetByTrigger` to command *execution*, which is a `POST`.
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
}
