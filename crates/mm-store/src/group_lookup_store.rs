//! Port of the `SqlGroupStore` reads and the one plain insert that only the plugin API reaches
//! (channels/store/sqlstore/group_store.go): `Create`, `GetByRemoteID`, `GetAllBySource`,
//! `GetByUser`, `GetMemberUsers` and `GetMemberUsersSortedPage`.
//!
//! A third trait on [`SqlGroupStore`], beside [`crate::GroupStore`] and
//! [`crate::GroupSyncableStore`], for the reason the syncable one gives: its callers are one
//! family (`PluginAPI`'s group methods, app/plugin_api.go:783-893) and the file boundary is the
//! seam. The api4 group reads that would also reach these are still forwarded when licensed.
//!
//! # `Create` is not `CreateWithUserIds` without the users
//!
//! Three differences a reader folding the two together would lose: `Create` inserts the
//! caller's **`DeleteAt`** where the other writes 0; it answers the caller's own value with only
//! `Id`, `CreateAt` and `UpdateAt` set — nothing is read back, so a `MemberCount` the caller
//! sent is answered unchanged; and a duplicate name is a **plain** error (Go's `errors.Wrapf`),
//! which the app layer answers 500 `app.insert_error`, not the 400 `app.custom_group.unique_name`
//! `CreateWithUserIds` raises.
//!
//! # A negative page is Postgres' refusal
//!
//! Go hands `uint64(perPage)` and `uint64(page * perPage)` to squirrel, which writes them as
//! literals; a negative value wraps to a number past `bigint`, and Postgres refuses the statement
//! with "bigint out of range" (measured on the stack's Postgres). So a negative page size or
//! offset is an error here, before any query, and the app layer's `app.select_error` follows.

use mm_model::group::{Group, GroupSource};
use mm_model::user::User;
use mm_model::utils::{get_millis, new_id};

use crate::error::StoreError;
use crate::group_store::{GroupRow, SqlGroupStore, is_group_name_unique_violation};
use crate::user_store::{UserRow, user_from_row};

/// The group reads and the plain insert of Go's `store.GroupStore` that only the plugin API
/// calls.
pub trait GroupLookupStore {
    /// Port of `SqlGroupStore.Create` (group_store.go:115); see the module note. An id already
    /// set is `ErrInvalidInput`, `IsValidForCreate` is the model's `AppError`.
    fn create(
        &self,
        group: Group,
    ) -> impl std::future::Future<Output = Result<Group, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetByRemoteID` (group_store.go:332): by remote id **and** source,
    /// deleted or not.
    fn get_by_remote_id(
        &self,
        remote_id: &str,
        source: &GroupSource,
    ) -> impl std::future::Future<Output = Result<Group, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetAllBySource` (group_store.go:350): the **live** groups of one
    /// source, in no promised order (Go has no `ORDER BY`).
    fn get_all_by_source(
        &self,
        source: &GroupSource,
    ) -> impl std::future::Future<Output = Result<Vec<Group>, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetByUser` (group_store.go:365): every group with a **live**
    /// membership row for the user — **deleted groups included**, since only the membership's
    /// `DeleteAt` is tested — and of the options only `FilterAllowReference`. No `ORDER BY`.
    fn get_by_user(
        &self,
        user_id: &str,
        filter_allow_reference: bool,
    ) -> impl std::future::Future<Output = Result<Vec<Group>, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetMemberUsers` (group_store.go:494): the live users with a live
    /// membership, in no promised order. `IsBot` is false for every row: Go's
    /// `getUsersColumns()` has no `Bots` join.
    fn get_member_users(
        &self,
        group_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetMemberUsersInTeam` (group_store.go:613): [`Self::get_member_users`]
    /// restricted to users with a live `TeamMembers` row on `team_id` (the `Teams` join only
    /// requires the team to exist). The one caller reads the ids.
    fn get_member_users_in_team(
        &self,
        group_id: &str,
        team_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetMemberUsersSortedPage` (group_store.go:515) as
    /// `GetMemberUsersPage` calls it: no view restrictions, `model.ShowUsername`, so ordered by
    /// username. See the module note for a negative page.
    fn get_member_users_page(
        &self,
        group_id: &str,
        page: i64,
        per_page: i64,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;
}

/// `groupMemberUsersSelectQuery` (group_store.go:85): `getUsersColumns()` with no bot columns,
/// so the three `UserRow` wants are constants.
macro_rules! member_user {
    ($row:expr) => {{
        let row = $row;
        user_from_row(UserRow {
            id: row.id,
            createat: row.createat,
            updateat: row.updateat,
            deleteat: row.deleteat,
            username: row.username,
            password: row.password,
            authdata: row.authdata,
            authservice: row.authservice,
            email: row.email,
            emailverified: row.emailverified,
            nickname: row.nickname,
            firstname: row.firstname,
            lastname: row.lastname,
            position: row.position,
            roles: row.roles,
            allowmarketing: row.allowmarketing,
            props: row.props,
            notifyprops: row.notifyprops,
            lastpasswordupdate: row.lastpasswordupdate,
            lastpictureupdate: row.lastpictureupdate,
            failedattempts: row.failedattempts,
            locale: row.locale,
            timezone: row.timezone,
            mfaactive: row.mfaactive,
            mfasecret: row.mfasecret,
            mfausedtimestamps: row.mfausedtimestamps,
            remoteid: row.remoteid,
            lastlogin: row.lastlogin,
            isbot: false,
            botdescription: String::new(),
            botlasticonupdate: 0,
        })
    }};
}

/// Go's `uint64(n)` of a negative `int`, which Postgres then refuses; see the module note.
fn unsigned(n: i64) -> Result<i64, StoreError> {
    if n < 0 {
        Err(StoreError::Argument {
            entity: "GroupMembers",
            detail: "bigint out of range",
        })
    } else {
        Ok(n)
    }
}

impl GroupLookupStore for SqlGroupStore {
    #[tracing::instrument(skip_all, fields(id))]
    async fn create(&self, mut group: Group) -> Result<Group, StoreError> {
        if !group.id.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "Group",
                field: "id",
                value: group.id,
            });
        }
        group
            .is_valid_for_create()
            .map_err(|app_error| StoreError::Invalid {
                entity: "Group",
                app_error,
            })?;

        group.id = new_id();
        group.create_at = get_millis();
        group.update_at = group.create_at;
        tracing::Span::current().record("id", &group.id);

        sqlx::query!(
            r#"
            INSERT INTO usergroups
                (id, name, displayname, description, source, remoteid, createat, updateat,
                 deleteat, allowreference)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
            group.id,
            group.name,
            group.display_name,
            group.description,
            group.source.as_str(),
            group.remote_id,
            group.create_at,
            group.update_at,
            group.delete_at,
            group.allow_reference
        )
        .execute(&self.pool)
        .await
        .map_err(|source| {
            let context = if is_group_name_unique_violation(&source) {
                format!("Group with name {} already exists", group.get_name())
            } else {
                "failed to save Group".to_owned()
            };
            StoreError::Db { context, source }
        })?;
        Ok(group)
    }

    #[tracing::instrument(skip(self), fields(remote_id = %remote_id, source = %source.as_str()))]
    async fn get_by_remote_id(
        &self,
        remote_id: &str,
        source: &GroupSource,
    ) -> Result<Group, StoreError> {
        let row = sqlx::query_as!(
            GroupRow,
            r#"
            SELECT id, name, displayname, description, source, remoteid, createat, updateat,
                   deleteat, allowreference
              FROM usergroups
             WHERE remoteid = $1 AND source = $2
            "#,
            remote_id,
            source.as_str()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Group with remoteId={remote_id}"),
            source,
        })?;
        row.map(GroupRow::into_group)
            .ok_or_else(|| StoreError::NotFound {
                entity: "Group",
                criteria: format!("remoteId={remote_id}"),
            })
    }

    #[tracing::instrument(skip(self), fields(source = %source.as_str(), found))]
    async fn get_all_by_source(&self, source: &GroupSource) -> Result<Vec<Group>, StoreError> {
        let rows = sqlx::query_as!(
            GroupRow,
            r#"
            SELECT id, name, displayname, description, source, remoteid, createat, updateat,
                   deleteat, allowreference
              FROM usergroups
             WHERE deleteat = 0 AND source = $1
            "#,
            source.as_str()
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| StoreError::Db {
            context: format!("failed to find Groups by groupSource={}", source.as_str()),
            source: err,
        })?;
        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(GroupRow::into_group).collect())
    }

    #[tracing::instrument(skip(self), fields(user_id = %user_id, filter_allow_reference, found))]
    async fn get_by_user(
        &self,
        user_id: &str,
        filter_allow_reference: bool,
    ) -> Result<Vec<Group>, StoreError> {
        let rows = sqlx::query_as!(
            GroupRow,
            r#"
            SELECT ug.id, ug.name, ug.displayname, ug.description, ug.source, ug.remoteid,
                   ug.createat, ug.updateat, ug.deleteat, ug.allowreference
              FROM usergroups ug
              JOIN groupmembers gm ON gm.groupid = ug.id
             WHERE gm.deleteat = 0 AND gm.userid = $1
               AND ($2 = FALSE OR ug.allowreference = TRUE)
            "#,
            user_id,
            filter_allow_reference
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Groups with userId={user_id}"),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());
        Ok(rows.into_iter().map(GroupRow::into_group).collect())
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id, found))]
    async fn get_member_users(&self, group_id: &str) -> Result<Vec<User>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT u.id, u.createat, u.updateat, u.deleteat, u.username, u.password, u.authdata,
                   u.authservice, u.email, u.emailverified, u.nickname, u.firstname, u.lastname,
                   u.position, u.roles, u.allowmarketing, u.props, u.notifyprops,
                   u.lastpasswordupdate, u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts, u.locale, u.timezone, u.mfaactive,
                   u.mfasecret, u.mfausedtimestamps, u.remoteid, u.lastlogin
              FROM groupmembers gm
              JOIN users u ON u.id = gm.userid
             WHERE gm.deleteat = 0 AND u.deleteat = 0 AND gm.groupid = $1
            "#,
            group_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find member Users for Group with id={group_id}"),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(|row| member_user!(row)).collect()
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id, team_id = %team_id, found))]
    async fn get_member_users_in_team(
        &self,
        group_id: &str,
        team_id: &str,
    ) -> Result<Vec<User>, StoreError> {
        let rows = sqlx::query!(
            r#"
            SELECT u.id, u.createat, u.updateat, u.deleteat, u.username, u.password, u.authdata,
                   u.authservice, u.email, u.emailverified, u.nickname, u.firstname, u.lastname,
                   u.position, u.roles, u.allowmarketing, u.props, u.notifyprops,
                   u.lastpasswordupdate, u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts, u.locale, u.timezone, u.mfaactive,
                   u.mfasecret, u.mfausedtimestamps, u.remoteid, u.lastlogin
              FROM groupmembers gm
              JOIN users u ON u.id = gm.userid
             WHERE gm.groupid = $1
               AND gm.userid IN (SELECT tm.userid FROM teammembers tm
                                   JOIN teams t ON t.id = tm.teamid
                                  WHERE tm.teamid = $2 AND tm.deleteat = 0)
               AND gm.deleteat = 0 AND u.deleteat = 0
            "#,
            group_id,
            team_id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to member Users for groupId={group_id} and teamId={team_id}"),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(|row| member_user!(row)).collect()
    }

    #[tracing::instrument(skip(self), fields(group_id = %group_id, page, per_page, found))]
    async fn get_member_users_page(
        &self,
        group_id: &str,
        page: i64,
        per_page: i64,
    ) -> Result<Vec<User>, StoreError> {
        let limit = unsigned(per_page)?;
        let offset = unsigned(page.wrapping_mul(per_page))?;
        let rows = sqlx::query!(
            r#"
            SELECT u.id, u.createat, u.updateat, u.deleteat, u.username, u.password, u.authdata,
                   u.authservice, u.email, u.emailverified, u.nickname, u.firstname, u.lastname,
                   u.position, u.roles, u.allowmarketing, u.props, u.notifyprops,
                   u.lastpasswordupdate, u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts, u.locale, u.timezone, u.mfaactive,
                   u.mfasecret, u.mfausedtimestamps, u.remoteid, u.lastlogin
              FROM groupmembers gm
              JOIN users u ON u.id = gm.userid
             WHERE gm.deleteat = 0 AND u.deleteat = 0 AND gm.groupid = $1
             ORDER BY u.username
             LIMIT $2 OFFSET $3
            "#,
            group_id,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find member Users for Group with id={group_id}"),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(|row| member_user!(row)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_negative_page_size_or_offset_is_refused_as_postgres_refuses_it() {
        assert!(unsigned(0).is_ok());
        assert_eq!(unsigned(7).ok(), Some(7));
        assert!(matches!(
            unsigned(-1),
            Err(StoreError::Argument {
                detail: "bigint out of range",
                ..
            })
        ));
    }
}
