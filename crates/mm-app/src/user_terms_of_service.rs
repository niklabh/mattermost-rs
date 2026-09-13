//! Port of `app/user_terms_of_service.go`, `GetUserTermsOfService` only.

use mm_model::user_terms_of_service::UserTermsOfService;
use mm_model::utils::{AppError, AppResult};
use mm_store::UserTermsOfServiceStore;

use crate::App;

impl App {
    /// Port of `app.App.GetUserTermsOfService` (user_terms_of_service.go:14).
    ///
    /// The 404 is a **normal outcome, not a failure**, at its one call site: `getUser` asks for
    /// every self-or-admin view and ignores a `StatusNotFound`, because most users have never
    /// accepted a terms of service — on Team Edition none can, since authoring one is licensed.
    /// The 404 id inserts `no_rows.` into the 500's id, the same one-word-apart shape as
    /// `GetChannelMember`'s `missing.`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn get_user_terms_of_service(&self, user_id: &str) -> AppResult<UserTermsOfService> {
        self.store()
            .user_terms_of_service()
            .get_by_user(user_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::boxed(
                        "GetUserTermsOfService",
                        "app.user_terms_of_service.get_by_user.no_rows.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "user terms of service lookup failed");
                    AppError::boxed(
                        "GetUserTermsOfService",
                        "app.user_terms_of_service.get_by_user.app_error",
                        None,
                        String::new(),
                        500,
                    )
                }
            })
    }
}

impl App {
    /// Port of `app.App.SaveUserTermsOfService` (user_terms_of_service.go:29).
    ///
    /// # `accepted` picks one of two different store calls, not a column value
    ///
    /// `true` upserts the acceptance row; `false` **deletes** it, matched on the user *and* the
    /// revision. So rejecting revision A leaves an acceptance of revision B standing, and
    /// rejecting a revision that was never accepted succeeds silently — the delete never checks
    /// how many rows it removed.
    ///
    /// # The two branches do not share an error id
    ///
    /// `app.user_terms_of_service.save.app_error` and
    /// `app.user_terms_of_service.delete.app_error`, both 500 and both under the same `where`.
    /// The save branch additionally lets a `*model.AppError` through untouched, which is how
    /// `UserTermsOfService.IsValid`'s **400** reaches a client — an empty `termsOfServiceId`
    /// posted with `accepted: true` is `model.user_terms_of_service.is_valid.
    /// terms_of_service_id.app_error`, not a 500. The delete branch has no such passthrough,
    /// because nothing on it validates.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, terms_id = %terms_of_service_id, accepted))]
    pub async fn save_user_terms_of_service(
        &self,
        user_id: &str,
        terms_of_service_id: &str,
        accepted: bool,
    ) -> AppResult {
        if accepted {
            let row = UserTermsOfService {
                user_id: user_id.to_owned(),
                terms_of_service_id: terms_of_service_id.to_owned(),
                create_at: 0,
            };
            self.store()
                .user_terms_of_service()
                .save(row)
                .await
                .map_err(|err| match err {
                    mm_store::StoreError::Invalid { app_error, .. } => app_error,
                    other => {
                        tracing::error!(error = %other, "user terms of service save failed");
                        AppError::boxed(
                            "SaveUserTermsOfService",
                            "app.user_terms_of_service.save.app_error",
                            None,
                            String::new(),
                            500,
                        )
                    }
                })?;
        } else {
            self.store()
                .user_terms_of_service()
                .delete(user_id, terms_of_service_id)
                .await
                .map_err(|err| {
                    tracing::error!(error = %err, "user terms of service delete failed");
                    AppError::boxed(
                        "SaveUserTermsOfService",
                        "app.user_terms_of_service.delete.app_error",
                        None,
                        String::new(),
                        500,
                    )
                })?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::App;
    use mm_store::SqlStore;
    use sqlx::postgres::PgPoolOptions;

    fn unreachable_app() -> App {
        let pool = PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        App::new(SqlStore::from_pool(pool))
    }

    /// A broken store is the 500 with the shorter id; the 404 inserts `no_rows.` and is pinned
    /// by contrast, because the unreachable store can only ever produce this branch.
    #[tokio::test]
    async fn a_broken_lookup_is_a_500_and_the_miss_id_inserts_no_rows() {
        let err = unreachable_app()
            .get_user_terms_of_service("uuuuuuuuuuuuuuuuuuuuuuuuuu")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.user_terms_of_service.get_by_user.app_error");
        assert_eq!(err.where_, "GetUserTermsOfService");
        assert!(err.params.is_none());

        let miss = "app.user_terms_of_service.get_by_user.no_rows.app_error";
        assert_ne!(
            err.id, miss,
            "the 404 id inserts no_rows. (user_terms_of_service.go:20)"
        );
    }
}
