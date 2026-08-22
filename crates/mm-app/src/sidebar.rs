//! Port of the sidebar-category reads of `app/channel_category.go`:
//! `GetSidebarCategoriesForTeamForUser` (:27), `GetSidebarCategories` (:50),
//! `GetSidebarCategoryOrder` (:73) and `GetSidebarCategory` (:90) — plus
//! `SessionHasPermissionToCategory` (app/authorization.go:242), which only these routes use.
//!
//! # One error id for all four
//!
//! Every branch of every function here produces `app.channel.sidebar_categories.app_error`.
//! Only the status code moves — 404 for a store not-found, 500 otherwise — and only `where_`
//! distinguishes the callers, which is `json:"-"` and never reaches a client. So a caller cannot
//! tell these apart from the body, and the *sole* observable difference between a missing
//! category and a broken query is the status line.

use mm_model::session::Session;
use mm_model::sidebar_category::{OrderedSidebarCategories, SidebarCategoryWithChannels};
use mm_model::utils::AppError;
use mm_store::{SidebarCategoryStore, StoreError};

use crate::App;

/// `app.channel.sidebar_categories.app_error` — the id every branch below carries.
const SIDEBAR_CATEGORIES_ERROR: &str = "app.channel.sidebar_categories.app_error";

/// What `GetSidebarCategoriesForTeamForUser` found, with Go's write branch called out rather
/// than silently dropped.
///
/// Go treats an empty result as *"the sidebar migration has not run for this user yet"* and
/// **creates the three default categories on the spot** — `createInitialSidebarCategories`
/// (channel_category.go:30), a transaction that also migrates the user's favourite-channel
/// preferences into `SidebarChannels`. That is a write, on a database the Go server owns, in a
/// handler this port serves read-only; reproducing it here would mean two servers racing to
/// insert the same deterministic ids.
///
/// So the empty case is reported rather than handled, and `mm_api::sidebar` forwards those
/// requests to Go, which performs the migration and answers. The result a client sees is Go's
/// own, which is the only answer that can be right.
///
/// Reachable in practice only where the rows are missing — a pre-5.32 account that has not
/// logged in since, or rows deleted underneath the server. Joining a team creates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidebarCategoriesResult {
    /// At least one category exists; this is Go's answer verbatim.
    Found(Box<OrderedSidebarCategories>),
    /// Zero categories. Go would create the defaults here; see the type's docs.
    NeedsInitialCategories,
}

impl App {
    /// Port of `app.App.GetSidebarCategoriesForTeamForUser` (channel_category.go:27).
    ///
    /// `GetSidebarCategories` (:50) is the same function under a second name, down to the error
    /// strings, and both call the same store method. Ported once.
    ///
    /// Note the order Go checks in: the **emptiness test runs before the error test**
    /// (`if err == nil && len(categories.Categories) == 0`), guarded by `err == nil` so a failed
    /// query cannot be mistaken for an empty one. Preserved here by matching on the `Result`.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id))]
    pub async fn get_sidebar_categories_for_team_for_user(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<SidebarCategoriesResult, AppError> {
        match self
            .store()
            .sidebar_category()
            .get_sidebar_categories(user_id, team_id)
            .await
        {
            Ok(categories) => {
                if categories
                    .categories
                    .as_ref()
                    .is_none_or(|categories| categories.is_empty())
                {
                    Ok(SidebarCategoriesResult::NeedsInitialCategories)
                } else {
                    Ok(SidebarCategoriesResult::Found(Box::new(categories)))
                }
            }
            Err(err) => Err(sidebar_categories_error(
                "GetSidebarCategoriesForTeamForUser",
                &err,
            )),
        }
    }

    /// Port of `app.App.GetSidebarCategoryOrder` (channel_category.go:73).
    ///
    /// **No empty fallback.** Unlike the two list functions above, Go does not create the default
    /// categories when this returns nothing — the same user, on the same team, gets three
    /// categories from `/categories` and `[]` from `/categories/order` in that state. Adding the
    /// fallback for symmetry would be a divergence, and an invisible one.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id))]
    pub async fn get_sidebar_category_order(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Vec<String>, AppError> {
        self.store()
            .sidebar_category()
            .get_sidebar_category_order(user_id, team_id)
            .await
            .map_err(|err| sidebar_categories_error("GetSidebarCategoryOrder", &err))
    }

    /// Port of `app.App.GetSidebarCategory` (channel_category.go:90).
    ///
    /// Takes no user or team: the category id alone identifies the row, and the ownership check
    /// lives in [`App::session_has_permission_to_category`] instead.
    #[tracing::instrument(skip(self), fields(category_id = %category_id))]
    pub async fn get_sidebar_category(
        &self,
        category_id: &str,
    ) -> Result<SidebarCategoryWithChannels, AppError> {
        self.store()
            .sidebar_category()
            .get_sidebar_category(category_id)
            .await
            .map_err(|err| sidebar_categories_error("GetSidebarCategory", &err))
    }

    /// Port of `app.App.SessionHasPermissionToCategory` (authorization.go:242).
    ///
    /// # It is not `SessionHasPermissionToUser` with a category on the end
    ///
    /// The two look interchangeable — both are the first gate of a handler in
    /// `api4/channel_category.go`, and both name `edit_other_users` in the refusal — but they
    /// share **only** the `edit_other_users` branch, and even that one differs:
    ///
    /// - There is **no `IsUnrestricted` branch and no `manage_system` branch**. A local-mode
    ///   caller is not waved through here.
    /// - There is **no self shortcut**. Asking about your own category still costs a query, and
    ///   still fails if the category is not actually yours.
    /// - The remaining branch is an *ownership* test on the row: the category must exist, and
    ///   its `UserId` must equal **both** the session's user and the `user_id` in the path, and
    ///   its `TeamId` must equal the path's team. `category.UserId` is compared twice, against
    ///   two different values, which is what stops a caller passing someone else's id in the
    ///   path and reading their own category through it — and vice versa.
    /// - A store error is **swallowed** (`err == nil && …`), so a missing category and a broken
    ///   database both deny. That is why `GET .../categories/{category_id}` answers **403** for a
    ///   category that does not exist: this gate refuses before `GetSidebarCategory`'s own 404
    ///   can be reached.
    #[tracing::instrument(skip(self, session), fields(actor = %session.user_id, category_id = %category_id))]
    pub async fn session_has_permission_to_category(
        &self,
        session: &Session,
        user_id: &str,
        team_id: &str,
        category_id: &str,
    ) -> bool {
        if self
            .session_has_permission_to(session, &mm_model::permission::PERMISSION_EDIT_OTHER_USERS)
            .await
        {
            return true;
        }

        // Go discards the error and falls through to the comparison against a nil category,
        // which is false. A lookup failure is a denial, not a 500.
        let Ok(category) = self.get_sidebar_category(category_id).await else {
            return false;
        };

        category.category.user_id == session.user_id
            && category.category.user_id == user_id
            && category.category.team_id == team_id
    }
}

/// The `AppError` every function in this module produces, with only the status code and the
/// (unwired) `where_` varying. Go's `switch` is `errors.As(err, &nfErr)` → 404, `default` → 500.
fn sidebar_categories_error(where_: &str, err: &StoreError) -> AppError {
    let not_found = err.is_not_found();
    if !not_found {
        // `?err` and not the `%err` the rest of this crate uses: `StoreError::Db`'s `Display` is
        // its `context` string alone, which for these queries is "failed to get categories for
        // userId=…" and says nothing about *why*. The `Debug` form carries the sqlx error — and
        // a `ColumnDecode { index: "9", UnexpectedNullError }` is the difference between a
        // diagnosable failure and an afternoon.
        tracing::error!(caller = where_, error = ?err, "sidebar category lookup failed");
    }
    AppError::new(
        where_,
        SIDEBAR_CATEGORIES_ERROR,
        None,
        String::new(),
        if not_found { 404 } else { 500 },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn not_found() -> StoreError {
        StoreError::NotFound {
            entity: "SidebarCategories",
            criteria: "id=y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
        }
    }

    fn broken_query() -> StoreError {
        StoreError::Db {
            context: "failed to get category".to_owned(),
            source: sqlx::Error::RowNotFound,
        }
    }

    /// Both branches carry the same id and differ only in the status line — the finding a caller
    /// most needs and the one the body cannot express.
    #[test]
    fn a_missing_category_and_a_broken_query_differ_only_in_status() {
        let missing = sidebar_categories_error("GetSidebarCategory", &not_found());
        let broken = sidebar_categories_error("GetSidebarCategory", &broken_query());

        assert_eq!(missing.id, SIDEBAR_CATEGORIES_ERROR);
        assert_eq!(broken.id, SIDEBAR_CATEGORIES_ERROR);
        assert_eq!(missing.status_code, 404);
        assert_eq!(broken.status_code, 500);
        assert_eq!(missing.detailed_error, "");
        assert!(missing.params.is_none(), "Go passes nil params here");
    }

    /// `where_` is the only thing separating the four callers, and it is `json:"-"`.
    #[test]
    fn the_caller_name_never_reaches_the_wire() {
        let err = sidebar_categories_error("GetSidebarCategoryOrder", &not_found());
        assert_eq!(err.where_, "GetSidebarCategoryOrder");
        let wire = serde_json::to_value(&err).expect("serialises");
        assert!(wire.get("where").is_none());
        assert!(wire.get("Where").is_none());
    }

    /// A `None` `categories` and an empty one both mean "run the migration". Go tests
    /// `len(categories.Categories) == 0`, and `len(nil) == 0` in Go — so a nil slice takes the
    /// same branch, not a panic and not the `Found` branch.
    #[test]
    fn nil_and_empty_category_lists_both_ask_for_initial_categories() {
        for categories in [None, Some(Vec::new())] {
            let ordered = OrderedSidebarCategories {
                categories,
                order: Some(Vec::new()),
            };
            assert!(
                ordered
                    .categories
                    .as_ref()
                    .is_none_or(|categories| categories.is_empty()),
                "both shapes are empty for Go's len() test"
            );
        }
    }
}
