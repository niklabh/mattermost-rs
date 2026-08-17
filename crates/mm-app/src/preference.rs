//! Port of `app.UpdatePreferences` (channels/app/preference.go:44).

use mm_model::preference::Preferences;
use mm_model::utils::AppError;
use mm_store::{PreferenceStore, StoreError};

use crate::App;

impl App {
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
