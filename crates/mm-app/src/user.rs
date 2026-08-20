//! Port of `app.GetUser` (channels/app/user.go).

use mm_model::user::User;
use mm_model::utils::AppError;
use mm_store::{StoreError, UserStore};

use crate::App;

impl App {
    /// Port of `app.App.GetUser`.
    ///
    /// Go returns `MissingAccountError` — id **`app.user.missing_account.const`**, 404 — for a
    /// miss, and a 500 for anything else. The two are not interchangeable at the API edge.
    ///
    /// Yes, `.const`: the id's last word is the Go keyword, not `error` (app/constants.go:7 —
    /// presumably a long-fossilised typo for a file of constants). This port shipped with
    /// `.error` for three days because `/users/me` can never miss — the session's user always
    /// exists — so no test could reach the branch until `GET /users/{user_id}` landed and its
    /// parity suite compared the 404 against the running server.
    #[tracing::instrument(skip_all, fields(user_id = %id))]
    pub async fn get_user(&self, id: &str) -> Result<User, AppError> {
        self.store().user().get(id).await.map_err(get_user_error)
    }
}

impl App {
    /// Port of `app.App.GetUserByUsername` (user.go:567).
    ///
    /// **Both branches carry the same id** — `app.user.get_by_username.app_error` — and only the
    /// status separates a miss from a broken query, the `GetChannelUnread` shape rather than
    /// `GetUser`'s two-id shape three lines up in the same Go file. Neither branch matches
    /// `MissingAccountError` either; a client cannot correlate "no such id" with "no such
    /// username" by error id, and that is Go's wire.
    #[tracing::instrument(skip_all, fields(username = %username))]
    pub async fn get_user_by_username(&self, username: &str) -> Result<User, AppError> {
        self.store()
            .user()
            .get_by_username(username)
            .await
            .map_err(|err| {
                let status = if matches!(err, StoreError::NotFound { .. }) {
                    404
                } else {
                    tracing::error!(error = %err, "user-by-username lookup failed");
                    500
                };
                AppError::new(
                    "GetUserByUsername",
                    "app.user.get_by_username.app_error",
                    None,
                    String::new(),
                    status,
                )
            })
    }
}

/// The store-error-to-`AppError` mapping for `GetUser`, split out so it is reachable from a test
/// without a database. A miss and a broken query are different HTTP statuses, and collapsing them
/// would report a server fault to the client as a missing account.
fn get_user_error(err: StoreError) -> AppError {
    match err {
        StoreError::NotFound { .. } => AppError::new(
            "GetUser",
            "app.user.missing_account.const",
            None,
            String::new(),
            404,
        ),
        other => {
            tracing::error!(error = %other, "user lookup failed");
            AppError::new(
                "GetUser",
                "app.user.get.app_error",
                None,
                String::new(),
                500,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_user_is_404_with_gos_error_id() {
        let err = get_user_error(StoreError::NotFound {
            entity: "User",
            criteria: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
        });
        assert_eq!(err.status_code, 404);
        assert_eq!(err.id, "app.user.missing_account.const");
    }

    /// A driver failure must not be reported to the client as a missing account — that would turn
    /// an outage into a plausible-looking 404 and hide it from every dashboard watching 5xx.
    #[test]
    fn a_broken_query_is_500_not_404() {
        let err = get_user_error(StoreError::Db {
            context: "connection pool closed".to_owned(),
            source: sqlx::Error::PoolClosed,
        });
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.user.get.app_error");
    }

    /// `GetUserByUsername` shares one id across both branches — only the status splits them —
    /// and that id is **not** `MissingAccountError`. The unreachable store can only produce the
    /// 500; the 404's identity is the same literal by construction, pinned by contrast.
    #[tokio::test]
    async fn a_broken_username_lookup_is_a_500_with_the_shared_id() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        let app = crate::App::new(mm_store::SqlStore::from_pool(pool));

        let err = app
            .get_user_by_username("sliceuser")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.user.get_by_username.app_error");
        assert_eq!(err.where_, "GetUserByUsername");
        assert_ne!(
            err.id, "app.user.missing_account.const",
            "the by-username miss does not wear MissingAccountError (user.go:573)"
        );
        assert!(err.params.is_none());
    }
}
