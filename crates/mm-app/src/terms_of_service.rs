//! Port of `App.GetLatestTermsOfService` (channels/app/terms_of_service.go:41).
//!
//! One store call and a two-way split, and the split is the whole content: **404 and 500 carry
//! different ids**, `app.terms_of_service.get.no_rows.app_error` and
//! `app.terms_of_service.get.app_error`. The webhook single reads share one id across the same
//! split and the OAuth single read has two ids one word apart; this one has two ids that do not
//! resemble each other at all. Three conventions in one tree.

use mm_model::terms_of_service::TermsOfService;
use mm_model::utils::{AppError, AppResult};
use mm_store::{StoreError, TermsOfServiceStore};

use crate::App;

impl App {
    /// Port of `App.GetLatestTermsOfService` (terms_of_service.go:41).
    ///
    /// Go passes `allowFromCache = true`, which reaches the cache layer's `"latest"` key
    /// (localcachelayer/terms_of_service_layer.go:47). Nothing here caches, so this is the
    /// cold-cache path — and the two agree whenever the cache and the table do, which
    /// `Save` keeps true by invalidating `"latest"` on every publish. Note the cache stores
    /// **only successes**: a not-found is not cached, so an empty table is re-queried every time
    /// on both servers.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn get_latest_terms_of_service(&self) -> AppResult<TermsOfService> {
        let terms = self
            .store()
            .terms_of_service()
            .get_latest()
            .await
            .map_err(latest_terms_error)?;

        tracing::Span::current().record("found", true);
        Ok(terms)
    }
}

/// Go's `errors.As(err, &nfErr)` split (terms_of_service.go:46-50).
fn latest_terms_error(err: StoreError) -> Box<AppError> {
    let not_found = err.is_not_found();
    if !not_found {
        tracing::error!(error = ?err, "terms of service lookup failed");
    }
    AppError::boxed(
        "GetLatestTermsOfService",
        if not_found {
            "app.terms_of_service.get.no_rows.app_error"
        } else {
            "app.terms_of_service.get.app_error"
        },
        None,
        String::new(),
        if not_found { 404 } else { 500 },
    )
}

impl App {
    /// Port of `app.App.CreateTermsOfService` (terms_of_service.go:14).
    ///
    /// # The user lookup happens *after* the struct is built and *before* the insert
    ///
    /// `GetUser(userID)` is the only thing standing between an unknown author and a published
    /// revision, and its **`AppError` is returned verbatim** — so posting terms as a deleted user
    /// answers `app.user.get.app_error` with a 404, not a terms-of-service id. A port that moved
    /// the check, or wrapped it, would change which error a client sees.
    ///
    /// # Three failures, and only one of them is reachable
    ///
    /// `Save` refuses a non-empty `Id` with `ErrInvalidInput` → 400
    /// `app.terms_of_service.create.existing.app_error`; passes `IsValid`'s `AppError` through;
    /// and turns anything else into 500 `app.terms_of_service.create.app_error`. The first cannot
    /// fire, because the struct built here always has an empty `Id` — and Go's own handling of it
    /// would **nil-dereference** if it did (`"id="+termsOfService.Id` reads the nil the failed
    /// `Save` just assigned). The detail is reproduced as the empty-id string it would have been.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, text_len = text.len(), terms_id))]
    pub async fn create_terms_of_service(
        &self,
        text: &str,
        user_id: &str,
    ) -> AppResult<TermsOfService> {
        let terms = TermsOfService {
            id: String::new(),
            create_at: 0,
            user_id: user_id.to_owned(),
            text: text.to_owned(),
        };

        self.get_user(user_id).await?;

        let saved = self
            .store()
            .terms_of_service()
            .save(terms)
            .await
            .map_err(create_terms_error)?;

        tracing::Span::current().record("terms_id", &saved.id);
        Ok(saved)
    }

    /// Port of `app.App.GetTermsOfService` (terms_of_service.go:56).
    ///
    /// Shares both error ids with [`App::get_latest_terms_of_service`] — `app.terms_of_service.
    /// get.no_rows.app_error` and `app.terms_of_service.get.app_error` — and differs only in the
    /// `where`. `saveUserTermsOfService` calls this purely to refuse an acceptance of a revision
    /// that does not exist, so the 404 is the one a client meets.
    #[tracing::instrument(skip(self), fields(terms_id = %id))]
    pub async fn get_terms_of_service(&self, id: &str) -> AppResult<TermsOfService> {
        self.store()
            .terms_of_service()
            .get(id)
            .await
            .map_err(|err| {
                let missing = err.is_not_found();
                if !missing {
                    tracing::error!(error = ?err, "terms of service lookup failed");
                }
                AppError::boxed(
                    "GetTermsOfService",
                    if missing {
                        "app.terms_of_service.get.no_rows.app_error"
                    } else {
                        "app.terms_of_service.get.app_error"
                    },
                    None,
                    String::new(),
                    if missing { 404 } else { 500 },
                )
            })
    }
}

/// Go's three-way `switch` on `Save`'s error (terms_of_service.go:26-34).
fn create_terms_error(err: StoreError) -> Box<AppError> {
    match err {
        StoreError::InvalidInput { .. } => AppError::boxed(
            "CreateTermsOfService",
            "app.terms_of_service.create.existing.app_error",
            None,
            // See the doc comment: Go reads the id off the nil it just assigned, so the only
            // value this can honestly carry is the empty one.
            "id=".to_owned(),
            400,
        ),
        StoreError::Invalid { app_error, .. } => app_error,
        other => {
            tracing::error!(error = ?other, "terms of service insert failed");
            AppError::boxed(
                "CreateTermsOfService",
                "app.terms_of_service.create.app_error",
                None,
                "terms_of_service_id=".to_owned(),
                500,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Two ids, and they are not variants of each other.** A port that reused one — as the
    /// webhook single reads legitimately do — would answer the wrong id for an empty table, which
    /// is the case a client actually branches on: no terms published yet.
    #[test]
    fn an_empty_table_and_a_broken_query_have_different_ids() {
        let missing = latest_terms_error(StoreError::NotFound {
            entity: "TermsOfService",
            criteria: "CreateAt=latest".to_owned(),
        });
        assert_eq!(missing.id, "app.terms_of_service.get.no_rows.app_error");
        assert_eq!(missing.status_code, 404);

        let broken = latest_terms_error(StoreError::Db {
            context: "boom".to_owned(),
            source: sqlx::Error::RowNotFound,
        });
        assert_eq!(broken.id, "app.terms_of_service.get.app_error");
        assert_eq!(broken.status_code, 500);

        assert_ne!(missing.id, broken.id);
    }
}
