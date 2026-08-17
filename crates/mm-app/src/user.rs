//! Port of `app.GetUser` (channels/app/user.go).

use mm_model::user::User;
use mm_model::utils::AppError;
use mm_store::{StoreError, UserStore};

use crate::App;

impl App {
    /// Port of `app.App.GetUser`.
    ///
    /// Go returns `MissingAccountError` — id `app.user.missing_account.error`, 404 — for a miss,
    /// and a 500 for anything else. The two are not interchangeable at the API edge.
    #[tracing::instrument(skip_all, fields(user_id = %id))]
    pub async fn get_user(&self, id: &str) -> Result<User, AppError> {
        self.store().user().get(id).await.map_err(get_user_error)
    }
}

/// The store-error-to-`AppError` mapping for `GetUser`, split out so it is reachable from a test
/// without a database. A miss and a broken query are different HTTP statuses, and collapsing them
/// would report a server fault to the client as a missing account.
fn get_user_error(err: StoreError) -> AppError {
    match err {
        StoreError::NotFound { .. } => AppError::new(
            "GetUser",
            "app.user.missing_account.error",
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
        assert_eq!(err.id, "app.user.missing_account.error");
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
}
