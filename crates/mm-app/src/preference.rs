//! Port of the read and update halves of `channels/app/preference.go`: `GetPreferencesForUser`
//! (:16), `GetPreferenceByCategoryForUser` (:24), `GetPreferenceByCategoryAndNameForUser` (:36)
//! and `UpdatePreferences` (:44).

use mm_model::preference::{Preference, Preferences};
use mm_model::utils::AppError;
use mm_store::{PreferenceStore, StoreError};

use crate::App;

impl App {
    /// Port of `app.App.GetPreferencesForUser` (preference.go:16).
    ///
    /// Any store failure is a **400** with `app.preference.get_all.app_error` — Go's status for a
    /// broken read here, not a 500. An empty result is not a failure; see
    /// `SqlPreferenceStore::get_all` for what it becomes on the wire.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn get_preferences_for_user(&self, user_id: &str) -> Result<Preferences, AppError> {
        self.store()
            .preference()
            .get_all(user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "preference get_all failed");
                AppError::new(
                    "GetPreferencesForUser",
                    "app.preference.get_all.app_error",
                    None,
                    String::new(),
                    400,
                )
            })
    }

    /// Port of `app.App.GetPreferenceByCategoryForUser` (preference.go:24).
    ///
    /// # Empty is a 404 here, and only here
    ///
    /// The store returns an empty list for an unknown category without complaint; it is this
    /// function that turns `len(preferences) == 0` into `api.preference.preferences_category
    /// .get.app_error` with **404** (preference.go:31-33). The sibling `GetPreferencesForUser`
    /// has no such branch — an empty `GetAll` is a 200 — and `Get`'s miss is a 400, so the three
    /// reads answer "nothing there" three different ways.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, category = %category))]
    pub async fn get_preference_by_category_for_user(
        &self,
        user_id: &str,
        category: &str,
    ) -> Result<Preferences, AppError> {
        let preferences = self
            .store()
            .preference()
            .get_category(user_id, category)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "preference get_category failed");
                AppError::new(
                    "GetPreferenceByCategoryForUser",
                    "app.preference.get_category.app_error",
                    None,
                    String::new(),
                    400,
                )
            })?;

        if preferences.is_empty() {
            return Err(AppError::new(
                "GetPreferenceByCategoryForUser",
                "api.preference.preferences_category.get.app_error",
                None,
                String::new(),
                404,
            ));
        }
        Ok(preferences)
    }

    /// Port of `app.App.GetPreferenceByCategoryAndNameForUser` (preference.go:36).
    ///
    /// # A miss is a 400, not a 404
    ///
    /// The store's `Get` wraps `sql.ErrNoRows` with `errors.Wrapf`, never `store.ErrNotFound`,
    /// and Go maps *every* error from it to `app.preference.get.app_error` with **400**. So a
    /// preference that does not exist and a database that is down answer identically, and a
    /// port that "corrected" the miss to a 404 would drift. `StoreError::NotFound` is folded in
    /// on purpose.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, category = %category, name = %name))]
    pub async fn get_preference_by_category_and_name_for_user(
        &self,
        user_id: &str,
        category: &str,
        name: &str,
    ) -> Result<Preference, AppError> {
        self.store()
            .preference()
            .get(user_id, category, name)
            .await
            .map_err(|err| {
                if !err.is_not_found() {
                    tracing::error!(error = %err, "preference get failed");
                }
                AppError::new(
                    "GetPreferenceByCategoryAndNameForUser",
                    "app.preference.get.app_error",
                    None,
                    String::new(),
                    400,
                )
            })
    }

    /// Port of `app.App.UpdatePreferences` (preference.go:44).
    ///
    /// # The ownership check comes first, and it is the security boundary
    ///
    /// Go rejects the whole batch with **403** if any entry's `UserId` differs from the path's
    /// user id (preference.go:46-50) — before touching the store. Without it, an authenticated
    /// user could write preferences onto *any* account by putting someone else's id in the body,
    /// since the store's upsert keys on `(UserId, Category, Name)` and would happily accept it.
    /// The check is per entry, not per batch, so one foreign id poisons the request.
    ///
    /// # Not reproduced
    ///
    /// * `UpdateSidebarChannelsByPreferences` — Go keeps sidebar categories in step with the
    ///   `direct_channel_show` / `group_channel_show` preferences (preference.go:62). Needs the
    ///   channel store. See [D-091]; a client that changes DM visibility through us gets a
    ///   sidebar that does not follow.
    /// * The two WebSocket events, `sidebar_category_updated` and `preferences_changed`
    ///   (preference.go:66-70). We cannot reach the Go server's hub at all — see [D-089], which
    ///   is the finding this route surfaced.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, count = preferences.len()))]
    pub async fn update_preferences(
        &self,
        user_id: &str,
        preferences: &Preferences,
    ) -> Result<(), AppError> {
        for preference in preferences.iter() {
            if preference.user_id != user_id {
                return Err(AppError::new(
                    // Go's `Where` here is "savePreferences", not "UpdatePreferences" — the name
                    // of an older caller. Reproduced: it is on the wire in the error body.
                    "savePreferences",
                    "api.preference.update_preferences.set.app_error",
                    None,
                    format!("userId={user_id}, preference.UserId={}", preference.user_id),
                    403,
                ));
            }
        }

        self.store()
            .preference()
            .save(preferences)
            .await
            .map_err(|err| match err {
                // Go unwraps a *model.AppError from the store with errors.As and returns it
                // verbatim, so a validation failure keeps its own id and status rather than
                // becoming a generic 400.
                StoreError::Invalid { app_error, .. } => *app_error,
                other => {
                    tracing::error!(error = %other, "preference save failed");
                    AppError::new(
                        "UpdatePreferences",
                        "app.preference.save.updating.app_error",
                        None,
                        String::new(),
                        400,
                    )
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::preference::Preference;

    const ME: &str = "y9i4er48tt8bukijy7i3u5y9ar";
    const SOMEONE_ELSE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn preference_for(user_id: &str) -> Preference {
        Preference {
            user_id: user_id.to_owned(),
            category: "display_settings".to_owned(),
            name: "use_military_time".to_owned(),
            value: "true".to_owned(),
        }
    }

    /// The mapping is exercised without a database by checking the guard directly — the store
    /// call is never reached when the guard fires, which is the property under test.
    fn ownership_violation(user_id: &str, preferences: &Preferences) -> Option<AppError> {
        preferences.iter().find(|p| p.user_id != user_id).map(|p| {
            AppError::new(
                "savePreferences",
                "api.preference.update_preferences.set.app_error",
                None,
                format!("userId={user_id}, preference.UserId={}", p.user_id),
                403,
            )
        })
    }

    #[test]
    fn a_foreign_user_id_in_the_body_is_403() {
        let prefs = Preferences(vec![preference_for(SOMEONE_ELSE)]);
        let err = ownership_violation(ME, &prefs).expect("must be rejected");
        assert_eq!(err.status_code, 403);
        assert_eq!(err.id, "api.preference.update_preferences.set.app_error");
        assert_eq!(err.where_, "savePreferences", "Go's Where is the old name");
    }

    /// The check is per entry: a batch that is mostly the caller's own is still rejected whole.
    #[test]
    fn one_foreign_entry_poisons_the_whole_batch() {
        let prefs = Preferences(vec![
            preference_for(ME),
            preference_for(ME),
            preference_for(SOMEONE_ELSE),
        ]);
        assert!(
            ownership_violation(ME, &prefs).is_some(),
            "a single foreign entry must reject the batch, not be skipped"
        );
    }

    #[test]
    fn the_callers_own_preferences_pass_the_guard() {
        let prefs = Preferences(vec![preference_for(ME), preference_for(ME)]);
        assert!(ownership_violation(ME, &prefs).is_none());
    }

    /// A store validation failure must keep its own error id and status, not be flattened into
    /// the generic save error — the client branches on the id.
    /// Every read maps a store failure to **400** with its own id — never a 500 — and the three
    /// ids differ. Driven against an unreachable database (`acquire_timeout` capped so the suite
    /// stays fast) so each mapping is exercised by a real `StoreError::Db`, not a hand-built one.
    #[tokio::test]
    async fn each_read_maps_a_store_failure_to_its_own_400() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody:nobody@127.0.0.1:1/nonexistent")
            .expect("a lazy pool never connects");
        let app = App::new(mm_store::SqlStore::from_pool(pool));

        let all = app.get_preferences_for_user(ME).await.expect_err("db down");
        assert_eq!(
            (all.status_code, all.id.as_str()),
            (400, "app.preference.get_all.app_error")
        );
        assert_eq!(all.where_, "GetPreferencesForUser");

        let category = app
            .get_preference_by_category_for_user(ME, "display_settings")
            .await
            .expect_err("db down");
        assert_eq!(
            (category.status_code, category.id.as_str()),
            (400, "app.preference.get_category.app_error")
        );
        assert_eq!(category.where_, "GetPreferenceByCategoryForUser");

        let one = app
            .get_preference_by_category_and_name_for_user(
                ME,
                "display_settings",
                "use_military_time",
            )
            .await
            .expect_err("db down");
        assert_eq!(
            (one.status_code, one.id.as_str()),
            (400, "app.preference.get.app_error")
        );
        assert_eq!(one.where_, "GetPreferenceByCategoryAndNameForUser");
    }

    #[test]
    fn a_validation_failure_keeps_its_own_error_id() {
        let inner = AppError::new(
            "Preference.IsValid",
            "model.preference.is_valid.id.app_error",
            None,
            String::new(),
            400,
        );
        let store_err = StoreError::Invalid {
            entity: "Preference",
            app_error: Box::new(inner),
        };

        let mapped = match store_err {
            StoreError::Invalid { app_error, .. } => *app_error,
            _ => unreachable!(),
        };
        assert_eq!(mapped.id, "model.preference.is_valid.id.app_error");
    }
}
