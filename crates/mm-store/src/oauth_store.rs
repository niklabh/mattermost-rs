//! Port of `SqlOAuthStore` (channels/store/sqlstore/oauth_store.go) — the three app reads the
//! `GET /api/v4/oauth/apps` routes need.
//!
//! The access-token and auth-code halves are not ported: nothing migrated performs an OAuth flow,
//! and a store function with no caller is a guess about a query nothing can falsify.

use mm_model::oauth::OAuthApp;
use mm_model::utils::StringArray;
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.OAuthStore` (store/store.go) that is ported.
pub trait OAuthStore {
    /// Port of `SqlOAuthStore.GetApps` (oauth_store.go:135) — every app, one page.
    fn get_apps(
        &self,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<OAuthApp>, StoreError>> + Send;

    /// Port of `SqlOAuthStore.GetAppByUser` (oauth_store.go:123) — one creator's apps.
    fn get_app_by_user(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<OAuthApp>, StoreError>> + Send;

    /// Port of `SqlOAuthStore.GetApp` (oauth_store.go:106).
    fn get_app(
        &self,
        id: &str,
    ) -> impl std::future::Future<Output = Result<OAuthApp, StoreError>> + Send;

    /// Port of `SqlOAuthStore.GetAuthorizedApps` (oauth_store.go:147) — the apps a user has
    /// granted, found by joining `Preferences`.
    /// Port of `SqlOAuthStore.SaveApp` (oauth_store.go).
    ///
    /// Go refuses an app that already carries an id, before `PreSave` runs.
    fn save_app(
        &self,
        app: &OAuthApp,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlOAuthStore.UpdateApp` (oauth_store.go).
    ///
    /// **Nine columns, and `CreatorId`/`CreateAt` are not among them** — Go re-reads the old row
    /// and copies both onto the struct first, so neither can be changed by an update.
    fn update_app(
        &self,
        app: &OAuthApp,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlOAuthStore.DeleteApp` (oauth_store.go) — a **hard** delete, and a cascade.
    ///
    /// Four statements in one transaction, in this order: the app, every `Sessions` row whose
    /// token appears in `OAuthAccessData` for it, the `OAuthAccessData` rows themselves, and the
    /// `Preferences` rows in the `oauth_app` category naming it. Deleting an OAuth app therefore
    /// **logs out everybody who authorised it**, which is why it is a transaction.
    fn delete_app(
        &self,
        app_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    fn get_authorized_apps(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<OAuthApp>, StoreError>> + Send;

    /// Port of `SqlOAuthStore.RemoveAllAccessData` (oauth_store.go:262).
    ///
    /// `DELETE FROM OAuthAccessData`, no predicate. The only caller is
    /// `RevokeSessionsFromAllUsers`, and Go's comment there says why it runs **first**: "revoke
    /// tokens before sessions so they can't be used to relogin" (platform/session.go:169). An
    /// OAuth access token outlives the session it minted, so deleting the sessions first would
    /// leave a window in which every revoked client can trade its token for a new one. The order
    /// is the security property; see [`crate::SessionStore::remove_all_sessions`] for the
    /// other half.
    fn remove_all_access_data(
        &self,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlOAuthStore {
    pool: PgPool,
}

impl SqlOAuthStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of `oAuthAppsSelectQuery` (oauth_store.go:29-32).
///
/// `CallbackUrls` is a `model.StringArray` — a JSON array inside a `varchar`, whose `Scan`
/// (model/utils.go:118) leaves the field **nil** for a SQL NULL. Same three states as the
/// outgoing webhooks' trigger words: `null`, `[]`, populated.
struct OAuthAppRow {
    id: String,
    creatorid: String,
    createat: i64,
    updateat: i64,
    clientsecret: String,
    name: String,
    description: String,
    iconurl: String,
    callbackurls: Option<String>,
    homepage: String,
    istrusted: bool,
    mattermostappid: String,
    isdynamicallyregistered: bool,
}

impl OAuthAppRow {
    fn into_model(self) -> Result<OAuthApp, StoreError> {
        let callback_urls =
            match self.callbackurls {
                None => None,
                Some(json) => Some(serde_json::from_str::<StringArray>(&json).map_err(
                    |source| StoreError::Decode {
                        entity: "OAuthApp",
                        column: "CallbackUrls",
                        source,
                    },
                )?),
            };
        Ok(OAuthApp {
            id: self.id,
            creator_id: self.creatorid,
            create_at: self.createat,
            update_at: self.updateat,
            client_secret: self.clientsecret,
            name: self.name,
            description: self.description,
            icon_url: self.iconurl,
            callback_urls,
            homepage: self.homepage,
            is_trusted: self.istrusted,
            mattermost_app_id: self.mattermostappid,
            is_dynamically_registered: self.isdynamicallyregistered,
        })
    }
}

/// The write half of the row's `CallbackUrls` decode: `None` becomes SQL `NULL`, keeping the
/// third state the model distinguishes from `[]`.
fn callback_urls_text(value: Option<&StringArray>) -> Result<Option<String>, StoreError> {
    value
        .map(|value| {
            serde_json::to_string(value).map_err(|source| StoreError::Decode {
                entity: "OAuthApp",
                column: "CallbackUrls",
                source,
            })
        })
        .transpose()
}

// The three queries below repeat the column list. Go builds all of them from one
// `SelectBuilder` (oauth_store.go:29), and a `macro_rules!` would say that here — but
// `sqlx::query_as!` needs a string **literal** to check against the database at compile time, so a
// macro-assembled query is not checked at all. Repetition that the compiler verifies beats reuse
// it cannot see.

impl OAuthStore for SqlOAuthStore {
    #[tracing::instrument(skip_all, fields(id = %app.id))]
    async fn save_app(&self, app: &OAuthApp) -> Result<(), StoreError> {
        let callback_urls = callback_urls_text(app.callback_urls.as_ref())?;
        sqlx::query!(
            r#"
            INSERT INTO oauthapps
                (id, creatorid, createat, updateat, clientsecret, name, description, iconurl,
                 callbackurls, homepage, istrusted, mattermostappid, isdynamicallyregistered)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            "#,
            app.id,
            app.creator_id,
            app.create_at,
            app.update_at,
            app.client_secret,
            app.name,
            app.description,
            app.icon_url,
            callback_urls,
            app.homepage,
            app.is_trusted,
            app.mattermost_app_id,
            app.is_dynamically_registered,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to save OAuthApp".to_owned(),
            source,
        })?;
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(id = %app.id))]
    async fn update_app(&self, app: &OAuthApp) -> Result<(), StoreError> {
        let callback_urls = callback_urls_text(app.callback_urls.as_ref())?;
        sqlx::query!(
            r#"
            UPDATE oauthapps
               SET updateat = $2, clientsecret = $3, name = $4, description = $5, iconurl = $6,
                   callbackurls = $7, homepage = $8, istrusted = $9, mattermostappid = $10,
                   isdynamicallyregistered = $11
             WHERE id = $1
            "#,
            app.id,
            app.update_at,
            app.client_secret,
            app.name,
            app.description,
            app.icon_url,
            callback_urls,
            app.homepage,
            app.is_trusted,
            app.mattermost_app_id,
            app.is_dynamically_registered,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update OAuthApp with id={}", app.id),
            source,
        })?;
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(id = %app_id))]
    async fn delete_app(&self, app_id: &str) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await.map_err(|source| StoreError::Db {
            context: "begin_transaction".to_owned(),
            source,
        })?;

        sqlx::query!("DELETE FROM oauthapps WHERE id = $1", app_id)
            .execute(&mut *tx)
            .await
            .map_err(|source| StoreError::Db {
                context: format!("failed to delete OAuthApp with id={app_id}"),
                source,
            })?;

        // **The sessions go before the tokens they are joined against.** Reversing these two
        // would leave every session issued through the app alive, because the `USING` join has
        // nothing left to match. Go's ordering, and the reason it is a transaction.
        sqlx::query!(
            r#"
            DELETE FROM sessions s
             USING oauthaccessdata o
             WHERE o.token = s.token AND o.clientid = $1
            "#,
            app_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to delete Session with OAuthAccessData.Id={app_id}"),
            source,
        })?;

        sqlx::query!("DELETE FROM oauthaccessdata WHERE clientid = $1", app_id)
            .execute(&mut *tx)
            .await
            .map_err(|source| StoreError::Db {
                context: format!("failed to delete OAuthAccessData with id={app_id}"),
                source,
            })?;

        // The `oauth_app` preference rows are what `getAuthorizedOAuthApps` reads, so an app that
        // survived here would keep appearing in every user's authorised list.
        sqlx::query!(
            "DELETE FROM preferences WHERE category = $1 AND name = $2",
            mm_model::preference::PREFERENCE_CATEGORY_AUTHORIZED_OAUTH_APP,
            app_id,
        )
        .execute(&mut *tx)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to delete Preferences with name={app_id}"),
            source,
        })?;

        tx.commit().await.map_err(|source| StoreError::Db {
            context: "commit_transaction".to_owned(),
            source,
        })
    }

    /// # No `ORDER BY`, and no `DeleteAt` either
    ///
    /// `OAuthApps` has no `DeleteAt` column at all — deleting an app removes the row — so unlike
    /// every other list this port serves there is nothing to filter. And the query has **no
    /// ordering**, so `LIMIT`/`OFFSET` page over the heap: two identical requests may return the
    /// same row twice or skip one if a row is inserted between them. That is Go's, and paging a
    /// table with no order is Go's bug to have, not ours to fix.
    #[tracing::instrument(skip_all, fields(offset, limit, found))]
    async fn get_apps(&self, offset: i64, limit: i64) -> Result<Vec<OAuthApp>, StoreError> {
        let rows = sqlx::query_as!(
            OAuthAppRow,
            r#"
            SELECT o.id                                  AS "id!",
                   COALESCE(o.creatorid, '')             AS "creatorid!",
                   COALESCE(o.createat, 0)               AS "createat!",
                   COALESCE(o.updateat, 0)               AS "updateat!",
                   COALESCE(o.clientsecret, '')          AS "clientsecret!",
                   COALESCE(o.name, '')                  AS "name!",
                   COALESCE(o.description, '')           AS "description!",
                   COALESCE(o.iconurl, '')               AS "iconurl!",
                   o.callbackurls                        AS "callbackurls?",
                   COALESCE(o.homepage, '')              AS "homepage!",
                   COALESCE(o.istrusted, FALSE)          AS "istrusted!",
                   o.mattermostappid                     AS "mattermostappid!",
                   COALESCE(o.isdynamicallyregistered, FALSE) AS "isdynamicallyregistered!"
              FROM oauthapps o
             LIMIT $1 OFFSET $2
            "#,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find OAuthApps".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(OAuthAppRow::into_model).collect()
    }

    /// The same query with `o.CreatorId = ?`. **Unconditional**, unlike the webhook stores' owner
    /// filters: Go adds it with a plain `Where`, so an empty user id here matches nothing rather
    /// than everything.
    #[tracing::instrument(skip_all, fields(user_id, offset, limit, found))]
    async fn get_app_by_user(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<OAuthApp>, StoreError> {
        let rows = sqlx::query_as!(
            OAuthAppRow,
            r#"
            SELECT o.id                                  AS "id!",
                   COALESCE(o.creatorid, '')             AS "creatorid!",
                   COALESCE(o.createat, 0)               AS "createat!",
                   COALESCE(o.updateat, 0)               AS "updateat!",
                   COALESCE(o.clientsecret, '')          AS "clientsecret!",
                   COALESCE(o.name, '')                  AS "name!",
                   COALESCE(o.description, '')           AS "description!",
                   COALESCE(o.iconurl, '')               AS "iconurl!",
                   o.callbackurls                        AS "callbackurls?",
                   COALESCE(o.homepage, '')              AS "homepage!",
                   COALESCE(o.istrusted, FALSE)          AS "istrusted!",
                   o.mattermostappid                     AS "mattermostappid!",
                   COALESCE(o.isdynamicallyregistered, FALSE) AS "isdynamicallyregistered!"
              FROM oauthapps o
             WHERE o.creatorid = $1
             LIMIT $2 OFFSET $3
            "#,
            user_id,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find OAuthApps with userId={user_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(OAuthAppRow::into_model).collect()
    }

    #[tracing::instrument(skip_all, fields(id, found))]
    async fn get_app(&self, id: &str) -> Result<OAuthApp, StoreError> {
        let row = sqlx::query_as!(
            OAuthAppRow,
            r#"
            SELECT o.id                                  AS "id!",
                   COALESCE(o.creatorid, '')             AS "creatorid!",
                   COALESCE(o.createat, 0)               AS "createat!",
                   COALESCE(o.updateat, 0)               AS "updateat!",
                   COALESCE(o.clientsecret, '')          AS "clientsecret!",
                   COALESCE(o.name, '')                  AS "name!",
                   COALESCE(o.description, '')           AS "description!",
                   COALESCE(o.iconurl, '')               AS "iconurl!",
                   o.callbackurls                        AS "callbackurls?",
                   COALESCE(o.homepage, '')              AS "homepage!",
                   COALESCE(o.istrusted, FALSE)          AS "istrusted!",
                   o.mattermostappid                     AS "mattermostappid!",
                   COALESCE(o.isdynamicallyregistered, FALSE) AS "isdynamicallyregistered!"
              FROM oauthapps o
             WHERE o.id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get OAuthApp with id={id}"),
            source,
        })?
        .ok_or_else(|| StoreError::NotFound {
            entity: "OAuthApp",
            criteria: format!("id={id}"),
        })?;

        tracing::Span::current().record("found", true);
        row.into_model()
    }

    /// # The join ignores the preference **category**
    ///
    /// `InnerJoin("Preferences AS p ON p.Name = o.Id AND p.UserId = ?")` (oauth_store.go:151) —
    /// and nothing else. Authorizing an app writes a preference in the `oauth_app` category, but
    /// the query never says so, so **any** preference row whose `Name` happens to equal an app id
    /// authorises that app for that user. Reproduced exactly: narrowing it to the category would
    /// be a security *improvement* that answers differently from the server we forward to.
    ///
    /// `Preferences` is keyed on `(UserId, Category, Name)`, so two categories naming the same app
    /// join twice and the app appears **twice** in the answer. That is Go's too — there is no
    /// `DISTINCT`.
    #[tracing::instrument(skip_all, fields(user_id, offset, limit, found))]
    async fn get_authorized_apps(
        &self,
        user_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<OAuthApp>, StoreError> {
        let rows = sqlx::query_as!(
            OAuthAppRow,
            r#"
            SELECT o.id                                  AS "id!",
                   COALESCE(o.creatorid, '')             AS "creatorid!",
                   COALESCE(o.createat, 0)               AS "createat!",
                   COALESCE(o.updateat, 0)               AS "updateat!",
                   COALESCE(o.clientsecret, '')          AS "clientsecret!",
                   COALESCE(o.name, '')                  AS "name!",
                   COALESCE(o.description, '')           AS "description!",
                   COALESCE(o.iconurl, '')               AS "iconurl!",
                   o.callbackurls                        AS "callbackurls?",
                   COALESCE(o.homepage, '')              AS "homepage!",
                   COALESCE(o.istrusted, FALSE)          AS "istrusted!",
                   o.mattermostappid                     AS "mattermostappid!",
                   COALESCE(o.isdynamicallyregistered, FALSE) AS "isdynamicallyregistered!"
              FROM oauthapps o
              JOIN preferences p ON p.name = o.id AND p.userid = $1
             LIMIT $2 OFFSET $3
            "#,
            user_id,
            limit,
            offset
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find OAuthApps with userId={user_id}"),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(OAuthAppRow::into_model).collect()
    }

    #[tracing::instrument(skip_all, fields(deleted))]
    async fn remove_all_access_data(&self) -> Result<(), StoreError> {
        let result = sqlx::query!("DELETE FROM oauthaccessdata")
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: "failed to delete OAuthAccessData".to_owned(),
                source,
            })?;

        tracing::Span::current().record("deleted", result.rows_affected());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every column lands on its field. `client_secret` and `homepage` are adjacent strings in
    /// neither the model's nor the table's order, which is exactly how a swap hides.
    #[test]
    fn a_row_maps_onto_the_model() {
        let app = OAuthAppRow {
            id: "mmrsoauth00000000000000001".to_owned(),
            creatorid: "6rtg4qbe5bn55mw5t6gphxyaxa".to_owned(),
            createat: 1_788_636_490_668,
            updateat: 1_788_636_490_669,
            clientsecret: "mmrssecret".to_owned(),
            name: "mmrs app".to_owned(),
            description: "a description".to_owned(),
            iconurl: "http://example.invalid/i.png".to_owned(),
            callbackurls: Some(r#"["http://example.invalid/cb"]"#.to_owned()),
            homepage: "http://example.invalid/".to_owned(),
            istrusted: true,
            mattermostappid: "mmrsappid".to_owned(),
            isdynamicallyregistered: true,
        }
        .into_model()
        .expect("the row converts");

        assert_eq!(app.id, "mmrsoauth00000000000000001");
        assert_eq!(app.creator_id, "6rtg4qbe5bn55mw5t6gphxyaxa");
        assert_eq!(app.create_at, 1_788_636_490_668);
        assert_eq!(app.update_at, 1_788_636_490_669);
        assert_eq!(app.client_secret, "mmrssecret");
        assert_eq!(app.name, "mmrs app");
        assert_eq!(app.description, "a description");
        assert_eq!(app.icon_url, "http://example.invalid/i.png");
        assert_eq!(
            app.callback_urls.as_deref(),
            Some(["http://example.invalid/cb".to_owned()].as_slice())
        );
        assert_eq!(app.homepage, "http://example.invalid/");
        assert!(app.is_trusted);
        assert_eq!(app.mattermost_app_id, "mmrsappid");
        assert!(app.is_dynamically_registered);
    }

    /// A NULL `CallbackUrls` is `None`, which reaches the wire as `null` — not `[]`.
    #[test]
    fn a_null_callback_urls_column_stays_none() {
        let app = OAuthAppRow {
            id: "mmrsoauth00000000000000002".to_owned(),
            creatorid: String::new(),
            createat: 0,
            updateat: 0,
            clientsecret: String::new(),
            name: String::new(),
            description: String::new(),
            iconurl: String::new(),
            callbackurls: None,
            homepage: String::new(),
            istrusted: false,
            mattermostappid: String::new(),
            isdynamicallyregistered: false,
        }
        .into_model()
        .expect("the row converts");
        assert!(app.callback_urls.is_none());
    }
}
