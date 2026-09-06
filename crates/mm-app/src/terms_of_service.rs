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
