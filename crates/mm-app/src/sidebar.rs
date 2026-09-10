//! Port of the sidebar-category reads **and writes** of `app/channel_category.go`:
//! `GetSidebarCategoriesForTeamForUser` (:27), `GetSidebarCategories` (:50),
//! `GetSidebarCategoryOrder` (:73), `GetSidebarCategory` (:90), `CreateSidebarCategory` (:105),
//! `UpdateSidebarCategoryOrder` (:122), `UpdateSidebarCategories` (:142),
//! `DeleteSidebarCategory` (:269) and `muteChannelsForUpdatedCategories` (:164) — plus
//! `SessionHasPermissionToCategory` (app/authorization.go:242), which only these routes use.
//!
//! # One error id for all four
//!
//! Every branch of every function here produces `app.channel.sidebar_categories.app_error` — the
//! writes included. Only the status code moves, and it moves differently per function: 404 for a
//! store not-found on the reads and on `CreateSidebarCategory`, **400** for the store's
//! `ErrInvalidInput` on the two functions that raise it, 500 for everything else, and 500 for
//! *everything* in `UpdateSidebarCategories`. Only `where_` distinguishes the callers, and it is
//! `json:"-"`, so a caller cannot tell these apart from the body: the sole observable difference
//! between a missing category and a broken query is the status line.

use mm_model::session::Session;
use mm_model::sidebar_category::{OrderedSidebarCategories, SidebarCategoryWithChannels};
use mm_model::utils::{AppError, AppResult};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_SIDEBAR_CATEGORY_CREATED, WEBSOCKET_EVENT_SIDEBAR_CATEGORY_DELETED,
    WEBSOCKET_EVENT_SIDEBAR_CATEGORY_ORDER_UPDATED, WEBSOCKET_EVENT_SIDEBAR_CATEGORY_UPDATED,
    WebSocketEvent,
};
use mm_store::{SidebarCategoryStore, StoreError};

use crate::App;

/// `app.channel.sidebar_categories.app_error` — the id every branch below carries.
const SIDEBAR_CATEGORIES_ERROR: &str = "app.channel.sidebar_categories.app_error";

impl App {
    /// Port of `app.App.createInitialSidebarCategories` (channel_category.go:18) — the lazy
    /// sidebar migration, reached from the read below and from nowhere else in this port.
    ///
    /// **Its error id is its own**, `app.channel.create_initial_sidebar_categories.internal_error`
    /// rather than the `app.channel.sidebar_categories.app_error` every other function in this
    /// module carries, and it has one branch: 500. So a `GET .../categories` that happens to
    /// trigger the migration and fail answers a body no other request on that route can produce.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id))]
    pub async fn create_initial_sidebar_categories(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> AppResult<OrderedSidebarCategories> {
        self.store()
            .sidebar_category()
            .create_initial_sidebar_categories(user_id, team_id)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "initial sidebar categories could not be created");
                AppError::boxed(
                    "createInitialSidebarCategories",
                    "app.channel.create_initial_sidebar_categories.internal_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetSidebarCategoriesForTeamForUser` (channel_category.go:27).
    ///
    /// `GetSidebarCategories` (:50) is the same function under a second name, down to the error
    /// strings, and both call the same store method. Ported once.
    ///
    /// Note the order Go checks in: the **emptiness test runs before the error test**
    /// (`if err == nil && len(categories.Categories) == 0`), guarded by `err == nil` so a failed
    /// query cannot be mistaken for an empty one. Preserved here by matching on the `Result`.
    ///
    /// # A `GET` that writes
    ///
    /// Zero categories means *"the sidebar migration has not run for this user yet"*, and Go
    /// **creates the three defaults on the spot** — inside the read, in a transaction that also
    /// migrates the user's `favorite_channel` preferences into `SidebarChannels`. So this read is
    /// a write on first contact, and the ids it mints are deterministic
    /// (`favorites_<userId>_<teamId>`) precisely so that two servers doing it at once converge on
    /// one set of rows rather than six.
    ///
    /// Until [`App::create_initial_sidebar_categories`] existed this case was forwarded to Go for
    /// exactly that reason. It no longer is: the deterministic ids and the primary key on
    /// `SidebarCategories.Id` are what make the race safe, and they are Go's own mechanism rather
    /// than something arranged here.
    ///
    /// Reachable in practice only where the rows are missing — a pre-5.32 account that has not
    /// logged in since, or rows deleted underneath the server. Joining a team creates them.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id, migrated))]
    pub async fn get_sidebar_categories_for_team_for_user(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> AppResult<OrderedSidebarCategories> {
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
                    tracing::Span::current().record("migrated", true);
                    self.create_initial_sidebar_categories(user_id, team_id)
                        .await
                } else {
                    tracing::Span::current().record("migrated", false);
                    Ok(categories)
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
    ) -> AppResult<Vec<String>> {
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
    ) -> AppResult<SidebarCategoryWithChannels> {
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
fn sidebar_categories_error(where_: &str, err: &StoreError) -> Box<AppError> {
    let not_found = err.is_not_found();
    if !not_found {
        // `?err` and not the `%err` the rest of this crate uses: `StoreError::Db`'s `Display` is
        // its `context` string alone, which for these queries is "failed to get categories for
        // userId=…" and says nothing about *why*. The `Debug` form carries the sqlx error — and
        // a `ColumnDecode { index: "9", UnexpectedNullError }` is the difference between a
        // diagnosable failure and an afternoon.
        tracing::error!(caller = where_, error = ?err, "sidebar category lookup failed");
    }
    AppError::boxed(
        where_,
        SIDEBAR_CATEGORIES_ERROR,
        None,
        String::new(),
        if not_found { 404 } else { 500 },
    )
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Which channels a category update implies should be muted or unmuted.
///
/// Split out of [`App::mute_channels_for_updated_categories`] so the decision can be tested
/// without a database — the write it feeds is not ported (see that function).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MuteReconciliation {
    pub to_mute: Vec<String>,
    pub to_unmute: Vec<String>,
}

impl App {
    /// Port of `app.App.CreateSidebarCategory` (channel_category.go:105).
    ///
    /// The store's not-found — "categories not found", raised when the user has no categories on
    /// this team at all — is a **404**; everything else is a 500. Both carry
    /// `app.channel.sidebar_categories.app_error`, as every function in this module does.
    ///
    /// # The event carries the id, not the category
    ///
    /// `sidebar_category_created` with `data["category_id"]`, addressed to the team **and** the
    /// user. `omit_connection_id` is empty — Go passes `""`, so unlike a draft or a preference
    /// write this event is *not* withheld from the connection that caused it, whatever
    /// `Connection-Id` the request carried. A client that assumed otherwise would double-apply.
    #[tracing::instrument(skip(self, new_category), fields(user_id = %user_id, team_id = %team_id))]
    pub async fn create_sidebar_category(
        &self,
        user_id: &str,
        team_id: &str,
        new_category: &SidebarCategoryWithChannels,
    ) -> AppResult<SidebarCategoryWithChannels> {
        let category = self
            .store()
            .sidebar_category()
            .create_sidebar_category(user_id, team_id, new_category)
            .await
            .map_err(|err| sidebar_categories_error("CreateSidebarCategory", &err))?;

        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_SIDEBAR_CATEGORY_CREATED,
            team_id,
            "",
            user_id,
            None,
            "",
        );
        message.add(
            "category_id",
            serde_json::Value::String(category.category.id.clone()),
        );
        self.publish(message).await;

        Ok(category)
    }

    /// Port of `app.App.UpdateSidebarCategoryOrder` (channel_category.go:122).
    ///
    /// # Three statuses out of one store call
    ///
    /// This is the only function in the module with a **400** branch: `store.ErrInvalidInput` —
    /// an order naming a category the user does not have — maps to 400, a not-found to 404, and
    /// anything else to 500. The wrong-length guard is a bare `errors.New` in Go, which
    /// `errors.As` matches against neither, so *that* one is a 500. See
    /// `mm_store::sidebar_category_store::update_sidebar_category_order`.
    ///
    /// # `data["order"]` is a JSON **array**
    ///
    /// `message.Add("order", categoryOrder)` passes the slice itself. Contrast
    /// `sidebar_category_updated` below, which adds a marshalled *string*. Two events in one file
    /// with two conventions, and a client parses them differently.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id, count = category_order.len()))]
    pub async fn update_sidebar_category_order(
        &self,
        user_id: &str,
        team_id: &str,
        category_order: &[String],
    ) -> AppResult<()> {
        self.store()
            .sidebar_category()
            .update_sidebar_category_order(user_id, team_id, category_order)
            .await
            .map_err(|err| {
                if err.is_invalid_input() {
                    tracing::debug!(error = %err, "sidebar category order refused");
                    AppError::boxed(
                        "UpdateSidebarCategoryOrder",
                        SIDEBAR_CATEGORIES_ERROR,
                        None,
                        String::new(),
                        400,
                    )
                } else {
                    sidebar_categories_error("UpdateSidebarCategoryOrder", &err)
                }
            })?;

        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_SIDEBAR_CATEGORY_ORDER_UPDATED,
            team_id,
            "",
            user_id,
            None,
            "",
        );
        message.add(
            "order",
            serde_json::Value::Array(
                category_order
                    .iter()
                    .map(|id| serde_json::Value::String(id.clone()))
                    .collect(),
            ),
        );
        self.publish(message).await;

        Ok(())
    }

    /// Port of `app.App.UpdateSidebarCategories` (channel_category.go:142) — behind both
    /// `PUT …/categories` and `PUT …/categories/{category_id}`.
    ///
    /// # Every store failure is a 500 here
    ///
    /// There is no `errors.As` switch: a category id naming no row produces
    /// `app.channel.sidebar_categories.app_error` with **500**, where the sibling functions would
    /// answer 404. Unreachable through the API — the handler's per-category permission gate
    /// answers 400 for an unknown id first — which is precisely why the difference is easy to
    /// "tidy up" and why it is written down here.
    ///
    /// # The event's payload is a JSON **string**
    ///
    /// `message.Add("updatedCategories", string(json.Marshal(updatedCategories)))` — a marshalled
    /// array inside a string, the same double encoding the draft events use. Adding the array
    /// itself would give every webapp a type error on a field it re-parses.
    ///
    /// # The publish happens *before* the mute reconciliation
    ///
    /// Go publishes, then calls `muteChannelsForUpdatedCategories`, which publishes its own
    /// `channel_member_updated` events. So the order on the wire is the category event first.
    #[tracing::instrument(skip(self, categories), fields(user_id = %user_id, team_id = %team_id, count = categories.len()))]
    pub async fn update_sidebar_categories(
        &self,
        user_id: &str,
        team_id: &str,
        categories: &[SidebarCategoryWithChannels],
    ) -> AppResult<Vec<SidebarCategoryWithChannels>> {
        let update = self
            .store()
            .sidebar_category()
            .update_sidebar_categories(user_id, team_id, categories)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "sidebar category update failed");
                AppError::boxed(
                    "UpdateSidebarCategories",
                    SIDEBAR_CATEGORIES_ERROR,
                    None,
                    String::new(),
                    500,
                )
            })?;

        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_SIDEBAR_CATEGORY_UPDATED,
            team_id,
            "",
            user_id,
            None,
            "",
        );
        // Go's `json.Marshal`, which escapes `<`, `>` and `&`. A display name carrying one of
        // those is the difference between our string and Go's.
        let encoded = mm_model::utils::go_json_marshal(&update.updated).map_err(|err| {
            tracing::error!(error = %err, "updated sidebar categories did not encode");
            AppError::boxed(
                "UpdateSidebarCategories",
                "api.marshal_error",
                None,
                String::new(),
                500,
            )
        })?;
        message.add("updatedCategories", serde_json::Value::String(encoded));
        self.publish(message).await;

        self.mute_channels_for_updated_categories(user_id, &update.updated, &update.original);

        Ok(update.updated)
    }

    /// Port of `app.App.DeleteSidebarCategory` (channel_category.go:269).
    ///
    /// **`ErrInvalidInput` → 400 is the interesting branch**, and it is the one a client actually
    /// hits: it is the store's refusal to delete a category whose type is not `custom`. So
    /// `DELETE …/categories/channels_<user>_<team>` is a 400 with
    /// `app.channel.sidebar_categories.app_error`, not a 403 and not a 405.
    #[tracing::instrument(skip(self), fields(user_id = %user_id, team_id = %team_id, category_id = %category_id))]
    pub async fn delete_sidebar_category(
        &self,
        user_id: &str,
        team_id: &str,
        category_id: &str,
    ) -> AppResult<()> {
        self.store()
            .sidebar_category()
            .delete_sidebar_category(category_id)
            .await
            .map_err(|err| {
                if err.is_invalid_input() {
                    tracing::debug!(error = %err, "refusing to delete a non-custom category");
                    AppError::boxed(
                        "DeleteSidebarCategory",
                        SIDEBAR_CATEGORIES_ERROR,
                        None,
                        String::new(),
                        400,
                    )
                } else {
                    tracing::error!(error = ?err, "sidebar category delete failed");
                    AppError::boxed(
                        "DeleteSidebarCategory",
                        SIDEBAR_CATEGORIES_ERROR,
                        None,
                        String::new(),
                        500,
                    )
                }
            })?;

        let mut message = WebSocketEvent::new(
            WEBSOCKET_EVENT_SIDEBAR_CATEGORY_DELETED,
            team_id,
            "",
            user_id,
            None,
            "",
        );
        message.add(
            "category_id",
            serde_json::Value::String(category_id.to_owned()),
        );
        self.publish(message).await;

        Ok(())
    }

    /// Port of `app.App.muteChannelsForUpdatedCategories` (channel_category.go:164) — **the
    /// decision only.**
    ///
    /// # What is not ported, and why it is not a silent gap
    ///
    /// Go finishes by calling `setChannelsMuted` (app/channel.go:4032), which reads the user's
    /// `ChannelMembers` rows, flips `notify_props["mark_unread"]` and writes them back through
    /// `Channel().UpdateMultipleMembers` — a `ChannelMembers` write that is not ported. So muting
    /// a category through this server changes the category's `muted` flag and leaves its channels'
    /// memberships alone, where Go would mute each of them and publish a
    /// `channel_member_updated` per channel. Recorded as [D-224].
    ///
    /// The *decision* is ported and tested because it is the part with branches, and because it
    /// is what the missing write will consume unchanged. It is computed on every update so the
    /// warning below names the channels a Go server would have touched.
    ///
    /// # Two independent sources of mutes
    ///
    /// 1. A category whose `muted` flag **changed** contributes all of its channels. The loop is
    ///    by **index** into the two slices, guarded by `i > len(originalCategories)-1` — the
    ///    slices are index-aligned with the request, so index is the identity here, not the id.
    /// 2. A channel that **moved** between two categories of differing `muted` contributes
    ///    itself. Go only considers channels that landed in one of the categories in this
    ///    request, deliberately: *"we don't worry about any channels that have moved outside of
    ///    these categories since that heavily complicates things"*.
    ///
    /// Both directions are collected, and a channel can appear in both lists — Go does not
    /// deduplicate across them, and `setChannelsMuted` then mutes and unmutes in sequence.
    #[tracing::instrument(skip(self, updated, original), fields(user_id = %user_id))]
    pub fn mute_channels_for_updated_categories(
        &self,
        user_id: &str,
        updated: &[SidebarCategoryWithChannels],
        original: &[SidebarCategoryWithChannels],
    ) {
        let reconciliation = mute_reconciliation(updated, original);
        if reconciliation.to_mute.is_empty() && reconciliation.to_unmute.is_empty() {
            return;
        }
        // Not `error!`: Go logs an error only when the write *fails*, and on a Go server this
        // path succeeds silently. The gap is ours, so it is reported as one.
        tracing::warn!(
            user_id = %user_id,
            to_mute = ?reconciliation.to_mute,
            to_unmute = ?reconciliation.to_unmute,
            "setChannelsMuted is not ported (D-224): these channel memberships keep their \
             previous mute state and no channel_member_updated was published"
        );
    }
}

/// The pure half of [`App::mute_channels_for_updated_categories`].
///
/// Kept a free function so every branch is reachable from a unit test with no `App` and no
/// database. See that method for what each branch means.
#[must_use]
pub fn mute_reconciliation(
    updated: &[SidebarCategoryWithChannels],
    original: &[SidebarCategoryWithChannels],
) -> MuteReconciliation {
    let mut out = MuteReconciliation::default();

    for (index, updated_category) in updated.iter().enumerate() {
        // Go: `if i > len(originalCategories)-1 { continue }`. Written as a lookup because
        // `len(...)-1` underflows for an empty slice in Rust where Go's int goes to -1.
        let Some(original_category) = original.get(index) else {
            continue;
        };

        let channels = updated_category.channel_ids.as_deref().unwrap_or_default();
        if updated_category.category.muted && !original_category.category.muted {
            out.to_mute.extend(channels.iter().cloned());
        } else if !updated_category.category.muted && original_category.category.muted {
            out.to_unmute.extend(channels.iter().cloned());
        }
    }

    for (channel_id, from_category_id, to_category_id) in
        diff_channels_between_categories(updated, original)
    {
        // Go indexes two maps built from the same two slices the diff came from, so both lookups
        // hit. A miss would be a nil dereference there; here it is a skip.
        let (Some(from), Some(to)) = (
            original
                .iter()
                .find(|category| category.category.id == from_category_id),
            updated
                .iter()
                .find(|category| category.category.id == to_category_id),
        ) else {
            continue;
        };

        if to.category.muted && !from.category.muted {
            out.to_mute.push(channel_id);
        } else if !to.category.muted && from.category.muted {
            out.to_unmute.push(channel_id);
        }
    }

    out
}

/// Port of `diffChannelsBetweenCategories` (channel_category.go:239): the channels whose category
/// changed, as `(channel_id, from_category_id, to_category_id)`.
///
/// # A channel that left the request's categories entirely is *not* reported
///
/// The condition is `originalCategoryId != updatedCategoryId && updatedCategoryId != ""`, so a
/// channel present in an original category and in none of the updated ones is skipped. That is
/// the "moved outside of these categories" case Go declines to handle.
///
/// # Later categories win when a channel appears twice
///
/// Both maps are built by iterating categories and overwriting per channel id, so a channel
/// listed in two categories of the same request is recorded against the **last** one. Go's map
/// iteration order makes the *result* order arbitrary, so the return here is sorted by channel id
/// to keep the two mute lists deterministic — the lists' contents are what
/// `setChannelsMuted` reads, and it treats them as sets.
fn diff_channels_between_categories(
    updated: &[SidebarCategoryWithChannels],
    original: &[SidebarCategoryWithChannels],
) -> Vec<(String, String, String)> {
    let map = |categories: &[SidebarCategoryWithChannels]| {
        let mut result: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for category in categories {
            for channel_id in category.channel_ids.as_deref().unwrap_or_default() {
                result.insert(channel_id.clone(), category.category.id.clone());
            }
        }
        result
    };

    let updated_map = map(updated);
    let original_map = map(original);

    let mut diff = Vec::new();
    for (channel_id, original_category_id) in original_map {
        let updated_category_id = updated_map.get(&channel_id).map_or("", String::as_str);
        if original_category_id != updated_category_id && !updated_category_id.is_empty() {
            diff.push((
                channel_id,
                original_category_id,
                updated_category_id.to_owned(),
            ));
        }
    }
    diff
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

    // -----------------------------------------------------------------------
    // muteChannelsForUpdatedCategories' decision (channel_category.go:164)
    // -----------------------------------------------------------------------

    fn category(id: &str, muted: bool, channels: &[&str]) -> SidebarCategoryWithChannels {
        SidebarCategoryWithChannels {
            category: mm_model::sidebar_category::SidebarCategory {
                id: id.to_owned(),
                muted,
                ..Default::default()
            },
            channel_ids: Some(channels.iter().map(|c| (*c).to_owned()).collect()),
        }
    }

    /// A category that gained `muted` contributes **all** of its channels, and one that lost it
    /// contributes all of them the other way. Both directions in one case, because a port that
    /// implemented only the first would pass a mute-only test.
    #[test]
    fn a_categorys_muted_flag_flipping_carries_all_of_its_channels() {
        let original = vec![
            category("a", false, &["c1", "c2"]),
            category("b", true, &["c3"]),
        ];
        let updated = vec![
            category("a", true, &["c1", "c2"]),
            category("b", false, &["c3"]),
        ];

        let out = mute_reconciliation(&updated, &original);
        assert_eq!(out.to_mute, vec!["c1".to_owned(), "c2".to_owned()]);
        assert_eq!(out.to_unmute, vec!["c3".to_owned()]);
    }

    /// A category whose flag did not move contributes nothing, whichever way the flag is set —
    /// the comparison is against the original and not against `false`.
    #[test]
    fn an_unchanged_flag_contributes_nothing() {
        for muted in [false, true] {
            let original = vec![category("a", muted, &["c1"])];
            let updated = vec![category("a", muted, &["c1"])];
            assert_eq!(
                mute_reconciliation(&updated, &original),
                MuteReconciliation::default(),
                "muted={muted}"
            );
        }
    }

    /// The pairing is **by index**, not by id: Go reads `originalCategories[i]` for
    /// `updatedCategories[i]`. Two categories in the reverse order make the two disagree, and
    /// this is the case that tells them apart.
    #[test]
    fn categories_are_paired_by_index_and_not_by_id() {
        // Index 0 is `a`(unmuted→) paired against `b`(muted): a false→? comparison by id would
        // find no change at all, while by index it is an unmute of `a`'s channels.
        let original = vec![category("b", true, &["c9"]), category("a", false, &["c1"])];
        let updated = vec![category("a", false, &["c1"]), category("b", true, &["c9"])];

        let out = mute_reconciliation(&updated, &original);
        assert_eq!(
            out.to_unmute,
            vec!["c1".to_owned()],
            "index 0 is updated `a` against original `b`"
        );
        assert_eq!(out.to_mute, vec!["c9".to_owned()]);
    }

    /// `if i > len(originalCategories)-1 { continue }`. A shorter original list must not panic,
    /// and in Rust `len()-1` on an empty slice underflows — which is why the port uses `get`.
    #[test]
    fn a_shorter_original_list_is_skipped_not_indexed() {
        let updated = vec![category("a", true, &["c1"]), category("b", true, &["c2"])];
        assert_eq!(
            mute_reconciliation(&updated, &[]),
            MuteReconciliation::default()
        );

        let original = vec![category("a", false, &["c1"])];
        let out = mute_reconciliation(&updated, &original);
        assert_eq!(out.to_mute, vec!["c1".to_owned()], "only the paired index");
    }

    /// A channel moving from an unmuted category into a muted one is muted on its own, even
    /// though neither category's flag changed. This is the second, independent source of mutes.
    #[test]
    fn a_channel_moving_into_a_muted_category_is_muted_by_itself() {
        let original = vec![
            category("loud", false, &["c1"]),
            category("quiet", true, &[]),
        ];
        let updated = vec![
            category("loud", false, &[]),
            category("quiet", true, &["c1"]),
        ];

        let out = mute_reconciliation(&updated, &original);
        assert_eq!(out.to_mute, vec!["c1".to_owned()]);
        assert!(out.to_unmute.is_empty());

        // And the reverse direction.
        let out = mute_reconciliation(&original, &updated);
        assert_eq!(out.to_unmute, vec!["c1".to_owned()]);
        assert!(out.to_mute.is_empty());
    }

    /// A move between two categories of the **same** mute state contributes nothing — the diff
    /// finds the move and both branches fail.
    #[test]
    fn a_move_between_equally_muted_categories_contributes_nothing() {
        let original = vec![category("x", true, &["c1"]), category("y", true, &[])];
        let updated = vec![category("x", true, &[]), category("y", true, &["c1"])];
        assert_eq!(
            mute_reconciliation(&updated, &original),
            MuteReconciliation::default()
        );
    }

    /// `updatedCategoryId != ""` — a channel that left the request's categories entirely is not
    /// reported. Go declines to handle it on purpose; dropping the guard would look up a category
    /// id of `""`, find nothing, and (in Go) dereference nil.
    #[test]
    fn a_channel_leaving_every_named_category_is_not_reported() {
        let original = vec![category("x", false, &["c1"]), category("y", true, &[])];
        let updated = vec![category("x", false, &[]), category("y", true, &[])];
        assert_eq!(
            mute_reconciliation(&updated, &original),
            MuteReconciliation::default(),
            "c1 is in no updated category, so there is no `to` to compare"
        );
    }

    /// Both sources firing at once: the category flag flipped **and** a channel moved in. Go does
    /// not deduplicate across the two lists, so the moved channel appears twice.
    #[test]
    fn the_two_sources_are_not_deduplicated_against_each_other() {
        let original = vec![category("x", false, &["c1"]), category("y", false, &["c2"])];
        let updated = vec![
            category("x", true, &["c1", "c2"]),
            category("y", false, &[]),
        ];

        let out = mute_reconciliation(&updated, &original);
        // `c1` and `c2` from the flag flip, then `c2` again from the move into a now-muted `x`.
        assert_eq!(
            out.to_mute,
            vec!["c1".to_owned(), "c2".to_owned(), "c2".to_owned()]
        );
    }

    /// A channel listed in two categories of the same request is recorded against the **last**
    /// one, because both maps are built by overwriting per channel id.
    #[test]
    fn the_last_category_listing_a_channel_wins_the_diff() {
        let original = vec![category("first", false, &["c1"])];
        let updated = vec![
            category("second", false, &["c1"]),
            category("third", true, &["c1"]),
        ];

        let out = mute_reconciliation(&updated, &original);
        assert_eq!(
            out.to_mute,
            vec!["c1".to_owned()],
            "`third` is muted and is the one recorded, so this is a mute"
        );
    }

    /// A `None` `channel_ids` is Go's nil slice: iterated zero times, not a panic.
    #[test]
    fn a_null_channel_list_contributes_nothing() {
        let mut original = category("a", false, &[]);
        original.channel_ids = None;
        let mut updated = category("a", true, &[]);
        updated.channel_ids = None;
        assert_eq!(
            mute_reconciliation(
                std::slice::from_ref(&updated),
                std::slice::from_ref(&original)
            ),
            MuteReconciliation::default()
        );
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
