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

impl App {
    /// Port of `app.App.CreateOAuthAppInternal` (app/oauth.go).
    ///
    /// `generate_secret` is `!request.IsPublic`: a **public** client keeps an empty secret, and
    /// that emptiness is what `IsPublicClient` later reads to refuse a regeneration. So the flag
    /// on the request is not stored anywhere — it survives only as the presence or absence of a
    /// secret.
    ///
    /// The feature gate's id is `api.oauth.register_oauth_app.turn_off.app_error`, which the
    /// other three write paths do **not** share: they use `api.oauth.allow_oauth.turn_off`.
    #[tracing::instrument(skip(self, app), fields(name = %app.name))]
    pub async fn create_oauth_app_internal(
        &self,
        app: &OAuthApp,
        generate_secret: bool,
    ) -> AppResult<OAuthApp> {
        if !self.config().enable_oauth_service_provider {
            return Err(AppError::boxed(
                "CreateOAuthApp",
                "api.oauth.register_oauth_app.turn_off.app_error",
                None,
                String::new(),
                501,
            ));
        }

        let mut app = app.clone();
        if generate_secret {
            app.client_secret = mm_model::utils::new_id();
        }

        // `SaveApp` refuses an app that already carries an id, before `PreSave` runs.
        if !app.id.is_empty() {
            return Err(AppError::boxed(
                "CreateOAuthApp",
                "app.oauth.save_app.existing.app_error",
                None,
                String::new(),
                400,
            ));
        }

        app.pre_save();
        app.is_valid()?;

        self.store().oauth().save_app(&app).await.map_err(|err| {
            tracing::error!(error = %err, name = %app.name, "Error saving OAuth app");
            AppError::boxed(
                "CreateOAuthApp",
                "app.oauth.save_app.save.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        Ok(app)
    }

    /// Port of `app.App.UpdateOAuthApp` (app/oauth.go).
    ///
    /// Five fields are copied off the old app and cannot be changed by a body: `Id`, `CreatorId`,
    /// `CreateAt`, **`ClientSecret`** and `IsDynamicallyRegistered`. The secret one matters — it
    /// is why an update cannot rotate a credential and `regen_secret` exists as its own route,
    /// which is the opposite of the outgoing-webhook update's behaviour.
    #[tracing::instrument(skip(self, old_app, updated_app), fields(id = %old_app.id))]
    pub async fn update_oauth_app(
        &self,
        old_app: &OAuthApp,
        updated_app: &OAuthApp,
    ) -> AppResult<OAuthApp> {
        if !self.config().enable_oauth_service_provider {
            return Err(oauth_disabled("UpdateOAuthApp"));
        }

        let mut app = updated_app.clone();
        app.id = old_app.id.clone();
        app.creator_id = old_app.creator_id.clone();
        app.create_at = old_app.create_at;
        app.client_secret = old_app.client_secret.clone();
        app.is_dynamically_registered = old_app.is_dynamically_registered;

        app.pre_update();
        app.is_valid()?;

        self.store()
            .oauth()
            .update_app(&app)
            .await
            .map_err(|err| update_app_error("UpdateOAuthApp", err))?;

        Ok(app)
    }

    /// Port of `app.App.DeleteOAuthApp` (app/oauth.go).
    ///
    /// Go follows the delete with `InvalidateAllCaches()`, which on a single node clears its own
    /// session cache — the app's sessions have just been deleted from under it. This server has
    /// no such cache and cannot reach Go's, so a session issued through a deleted app stays live
    /// in Go's memory until it expires. See [D-190]; it is the sharpest instance of that entry so
    /// far, because the row really is gone.
    #[tracing::instrument(skip(self), fields(id = %app_id))]
    pub async fn delete_oauth_app(&self, app_id: &str) -> AppResult<()> {
        if !self.config().enable_oauth_service_provider {
            return Err(oauth_disabled("DeleteOAuthApp"));
        }

        self.store()
            .oauth()
            .delete_app(app_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "oauth app delete failed");
                AppError::boxed(
                    "DeleteOAuthApp",
                    "app.oauth.delete_app.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.RegenerateOAuthAppSecret` (app/oauth.go).
    ///
    /// A new secret and nothing else, through the same `UpdateApp` — whose `SET` list covers
    /// `ClientSecret`, which is exactly what `UpdateOAuthApp` above relies on *not* changing.
    #[tracing::instrument(skip(self, app), fields(id = %app.id))]
    pub async fn regenerate_oauth_app_secret(&self, app: &OAuthApp) -> AppResult<OAuthApp> {
        if !self.config().enable_oauth_service_provider {
            return Err(oauth_disabled("RegenerateOAuthAppSecret"));
        }

        let mut app = app.clone();
        app.client_secret = mm_model::utils::new_id();

        app.pre_update();
        app.is_valid()?;

        self.store()
            .oauth()
            .update_app(&app)
            .await
            .map_err(|err| update_app_error("RegenerateOAuthAppSecret", err))?;

        Ok(app)
    }
}

/// The id the three non-create write paths share, at 501 — **different from the create path's**,
/// which is `api.oauth.register_oauth_app.turn_off.app_error`.
fn oauth_disabled(where_: &'static str) -> Box<AppError> {
    AppError::boxed(
        where_,
        "api.oauth.allow_oauth.turn_off.app_error",
        None,
        String::new(),
        501,
    )
}

fn update_app_error(where_: &'static str, err: StoreError) -> Box<AppError> {
    tracing::error!(error = %err, "oauth app update failed");
    AppError::boxed(
        where_,
        "app.oauth.update_app.updating.app_error",
        None,
        String::new(),
        500,
    )
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
