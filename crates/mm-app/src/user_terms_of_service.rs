//! Port of `app/user_terms_of_service.go`, `GetUserTermsOfService` only.

use mm_model::user_terms_of_service::UserTermsOfService;
use mm_model::utils::AppError;
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
    pub async fn get_user_terms_of_service(
        &self,
        user_id: &str,
    ) -> Result<UserTermsOfService, AppError> {
        self.store()
            .user_terms_of_service()
            .get_by_user(user_id)
            .await
            .map_err(|err| {
                if err.is_not_found() {
                    AppError::new(
                        "GetUserTermsOfService",
                        "app.user_terms_of_service.get_by_user.no_rows.app_error",
                        None,
                        String::new(),
                        404,
                    )
                } else {
                    tracing::error!(error = %err, "user terms of service lookup failed");
                    AppError::new(
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
