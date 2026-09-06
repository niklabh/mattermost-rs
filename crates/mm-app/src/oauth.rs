//! Port of the three `App` functions behind the OAuth **app** reads (channels/app/oauth.go:74,
//! :137, :150).
//!
//! Each opens with the same config gate answering **501**, and each names a different error id
//! for its store failure. The single-app read splits 404 from 500 with **two different ids** —
//! unlike the webhook single reads, which share one id and change only the status. Two files in
//! the same tree, two conventions; both are reproduced.

use mm_model::oauth::OAuthApp;
use mm_model::utils::{AppError, AppResult};
use mm_store::{OAuthStore, StoreError};

use crate::App;

/// `api.oauth.allow_oauth.turn_off.app_error` — a **501** from all three entry points.
const DISABLED_ERROR: &str = "api.oauth.allow_oauth.turn_off.app_error";

impl App {
    /// Port of `App.GetOAuthApps` (oauth.go:137).
    #[tracing::instrument(skip_all, fields(page, per_page, found))]
    pub async fn get_oauth_apps(&self, page: i64, per_page: i64) -> AppResult<Vec<OAuthApp>> {
        if !self.config().enable_oauth_service_provider {
            return Err(disabled("GetOAuthApps"));
        }

        let apps = self
            .store()
            .oauth()
            .get_apps(page * per_page, per_page)
            .await
            .map_err(|err| failure("GetOAuthApps", "app.oauth.get_apps.find.app_error", err))?;

        tracing::Span::current().record("found", apps.len());
        Ok(apps)
    }

    /// Port of `App.GetOAuthAppsByCreator` (oauth.go:150).
    ///
    /// Its **disabled** error names `GetOAuthAppsByUser` while its store failure names
    /// `GetOAuthAppsByCreator` — two `where`s for one function, and neither matches the other.
    /// Reproduced verbatim; `where_` is the only field separating two near-identical failures in a
    /// log.
    #[tracing::instrument(skip_all, fields(user_id, page, per_page, found))]
    pub async fn get_oauth_apps_by_creator(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<OAuthApp>> {
        if !self.config().enable_oauth_service_provider {
            return Err(disabled("GetOAuthAppsByUser"));
        }

        let apps = self
            .store()
            .oauth()
            .get_app_by_user(user_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                failure(
                    "GetOAuthAppsByCreator",
                    "app.oauth.get_app_by_user.find.app_error",
                    err,
                )
            })?;

        tracing::Span::current().record("found", apps.len());
        Ok(apps)
    }

    /// Port of `App.GetOAuthApp` (oauth.go:74).
    ///
    /// **Two ids, one word apart**: `app.oauth.get_app.find.app_error` at 404 and
    /// `…get_app.finding.app_error` at 500. The webhook single reads share one id across the same
    /// split; this one does not, and swapping them is a wire change a reader would never notice.
    #[tracing::instrument(skip_all, fields(app_id))]
    pub async fn get_oauth_app(&self, app_id: &str) -> AppResult<OAuthApp> {
        if !self.config().enable_oauth_service_provider {
            return Err(disabled("GetOAuthApp"));
        }

        self.store().oauth().get_app(app_id).await.map_err(|err| {
            let not_found = err.is_not_found();
            if !not_found {
                tracing::error!(error = ?err, "oauth app lookup failed");
            }
            AppError::boxed(
                "GetOAuthApp",
                if not_found {
                    "app.oauth.get_app.find.app_error"
                } else {
                    "app.oauth.get_app.finding.app_error"
                },
                None,
                String::new(),
                if not_found { 404 } else { 500 },
            )
        })
    }

    /// Port of `App.GetAuthorizedAppsForUser` (oauth.go:632).
    ///
    /// **This list is sanitised and the admin one is not.** Every app goes through `Sanitize()`
    /// before it is returned (oauth.go:642-645), so a user reading their own authorisations never
    /// sees a client secret — while `GetOAuthApps`, five hundred lines away in the same file,
    /// hands every secret to an admin. The sanitising lives in the *app* layer here and in the
    /// *handler* for `/info`; both are ported where Go put them, because "where" is what a reader
    /// checks.
    #[tracing::instrument(skip_all, fields(user_id, page, per_page, found))]
    pub async fn get_authorized_apps_for_user(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<OAuthApp>> {
        if !self.config().enable_oauth_service_provider {
            return Err(disabled("GetAuthorizedAppsForUser"));
        }

        let mut apps = self
            .store()
            .oauth()
            .get_authorized_apps(user_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                failure(
                    "GetAuthorizedAppsForUser",
                    // The **same id as `GetOAuthApps`**, not the `by_user` one its neighbour uses
                    // — Go reaches for `get_apps.find` here (oauth.go:639).
                    "app.oauth.get_apps.find.app_error",
                    err,
                )
            })?;

        for app in &mut apps {
            // `OAuthApp.Sanitize` (model/oauth.go:164): one field.
            app.client_secret = String::new();
        }

        tracing::Span::current().record("found", apps.len());
        Ok(apps)
    }
}

fn disabled(where_: &str) -> Box<AppError> {
    AppError::boxed(where_, DISABLED_ERROR, None, String::new(), 501)
}

/// The two list functions' 500s: **different ids**, same status.
fn failure(where_: &str, id: &'static str, err: StoreError) -> Box<AppError> {
    tracing::error!(caller = where_, error = ?err, "oauth app lookup failed");
    AppError::boxed(where_, id, None, String::new(), 500)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One id and one status for the gate, three different `where`s — and the third is
    /// `GetOAuthAppsByUser`, which is not the name of any function.
    #[test]
    fn the_disabled_gate_is_a_501_from_three_wheres() {
        for where_ in ["GetOAuthApps", "GetOAuthAppsByUser", "GetOAuthApp"] {
            let err = disabled(where_);
            assert_eq!(err.status_code, 501, "{where_}");
            assert_eq!(err.id, DISABLED_ERROR, "{where_}");
            assert_eq!(err.where_, where_);
        }
    }

    /// The two list failures do not share an id, and neither is the single read's.
    #[test]
    fn the_three_failure_ids_are_distinct() {
        let store_error = || StoreError::Db {
            context: "boom".to_owned(),
            source: sqlx::Error::RowNotFound,
        };
        let all = failure(
            "GetOAuthApps",
            "app.oauth.get_apps.find.app_error",
            store_error(),
        );
        let mine = failure(
            "GetOAuthAppsByCreator",
            "app.oauth.get_app_by_user.find.app_error",
            store_error(),
        );
        assert_ne!(all.id, mine.id);
        assert_eq!(all.status_code, 500);
        assert_eq!(mine.status_code, 500);
        assert_ne!(all.id, "app.oauth.get_app.find.app_error");
    }
}
