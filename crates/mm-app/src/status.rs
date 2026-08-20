//! Port of `app.GetUserStatusesByIds` (channels/app/status.go:16), which is one line over
//! `PlatformService.GetUserStatusesByIds` (channels/app/platform/status.go:136).

use mm_model::status::{STATUS_OFFLINE, Status};
use mm_model::utils::AppError;
use mm_store::StatusStore;

use crate::App;

/// Stand-in for `ServiceSettings.EnableUserStatuses` (config.go:712 defaults it to `true`)
/// until config is ported — the same arrangement as the privacy settings ([D-085]).
///
/// Observable when flipped: both status routes answer as if nobody had a status row — the list
/// route with `[]` and the single-user route with a 404 — rather than with "offline" for each.
pub const ENABLE_USER_STATUSES: bool = true;

impl App {
    /// Port of `PlatformService.GetUserStatusesByIds` (platform/status.go:136).
    ///
    /// # What the cache means for the port
    ///
    /// Go consults `statusCache` first and reads the database only for the misses. The cache is
    /// not ported; every id is a miss here, and `GetByIds` answers the lot. That is a faithful
    /// port of the **cold-cache** path, and the *content* matches whenever the cache and the
    /// table agree — which `SaveAndBroadcastStatus` keeps true for every status written over
    /// REST (`PUT /users/{id}/status`). Where they disagree the difference is Go's own:
    /// `SetActiveChannel` (app/channel.go:3158) and the websocket presence paths update the cache
    /// without writing the row, so a user Go has seen recently may read `online` there and
    /// `away`/`offline` here — plus a leaked `active_channel` key, since api4 writes the cached
    /// object with `json.Marshal` rather than `ToJSON`. That gap is a cache-state property, not a
    /// wire-format one; see the route notes in `MIGRATION.md`.
    ///
    /// # Order
    ///
    /// Go appends cache hits in input order, then the database rows in whatever order the query
    /// returned them, then the synthesised statuses in input order. On a warm cache — the state
    /// every request after the first sees — that is "found, in input order; then missing, in
    /// input order", and that is the order produced here: [`merge_with_offline`] sorts the rows
    /// by id (the input is already sorted, see `SortedArrayFromJSON`), so a store that returns
    /// heap order cannot leak it onto the wire.
    ///
    /// # Missing rows are not an error
    ///
    /// A user with no `Status` row — and equally an id that belongs to **no user at all** — is
    /// reported as `{user_id, status: "offline"}` with every other field zero. Go's own comment
    /// says so ("This also return the status offline for the non-existing Ids"); the single-user
    /// route therefore answers 200 for an unknown id, and its 404 branch fires only when the
    /// feature is disabled.
    #[tracing::instrument(skip_all, fields(asked = user_ids.len()))]
    pub async fn get_user_statuses_by_ids(
        &self,
        user_ids: &[String],
    ) -> Result<Vec<Status>, AppError> {
        if !ENABLE_USER_STATUSES {
            return Ok(Vec::new());
        }

        let found = self
            .store()
            .status()
            .get_by_ids(user_ids)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "status lookup failed");
                AppError::new(
                    "GetUserStatusesByIds",
                    "app.status.get.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(merge_with_offline(user_ids, found))
    }
}

/// The tail of `GetUserStatusesByIds` (platform/status.go:185-199): the rows that came back,
/// then an offline status for every asked-for id that did not.
///
/// Go removes from `missingUserIds` each id that appears in the result, then appends a
/// `&model.Status{UserId: userID, Status: "offline"}` for what is left — `Manual` false,
/// `LastActivityAt` zero, `DNDEndTime` zero. Reproduced as a two-pass membership check rather
/// than Go's in-place splice, same outcome.
fn merge_with_offline(user_ids: &[String], mut found: Vec<Status>) -> Vec<Status> {
    // Warm-cache order: see the method doc. `sort_by` (stable) rather than `sort_unstable_by`
    // so that two rows with the same id — impossible under the primary key, but cheap to be
    // deterministic about — keep the store's relative order.
    found.sort_by(|a, b| a.user_id.cmp(&b.user_id));

    let missing: Vec<&String> = user_ids
        .iter()
        .filter(|id| !found.iter().any(|status| &status.user_id == *id))
        .collect();

    found.extend(missing.into_iter().map(|id| Status {
        user_id: id.to_owned(),
        status: STATUS_OFFLINE.to_owned(),
        ..Default::default()
    }));

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(user_id: &str, state: &str) -> Status {
        Status {
            user_id: user_id.to_owned(),
            status: state.to_owned(),
            manual: true,
            last_activity_at: 1_701_355_039_000,
            dnd_end_time: 58,
            ..Default::default()
        }
    }

    /// The synthesised status is exactly `{UserId, Status: "offline"}` — every other field at
    /// its zero value, not copied from anywhere.
    #[test]
    fn a_missing_row_becomes_a_zeroed_offline_status() {
        let ids = vec!["aaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()];
        let merged = merge_with_offline(&ids, Vec::new());

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].user_id, "aaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(merged[0].status, "offline");
        assert!(!merged[0].manual);
        assert_eq!(merged[0].last_activity_at, 0);
        assert_eq!(merged[0].dnd_end_time, 0);
        assert_eq!(merged[0].active_channel, "");
        assert_eq!(merged[0].prev_status, "");
    }

    /// Found first in id order, then the missing in input order — and a found id is never
    /// *also* synthesised.
    #[test]
    fn found_rows_come_first_then_the_missing_in_input_order() {
        let ids: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.repeat(26)).collect();
        // The store's order is deliberately not the input's.
        let found = vec![
            status(&"d".repeat(26), "away"),
            status(&"b".repeat(26), "dnd"),
        ];

        let merged = merge_with_offline(&ids, found);
        let order: Vec<(&str, &str)> = merged
            .iter()
            .map(|s| (&s.user_id[..1], s.status.as_str()))
            .collect();

        assert_eq!(
            order,
            vec![
                ("b", "dnd"),
                ("d", "away"),
                ("a", "offline"),
                ("c", "offline")
            ]
        );
    }

    /// A found row keeps every field the store gave it — the merge adds, it does not rewrite.
    #[test]
    fn a_found_row_is_passed_through_untouched() {
        let ids = vec!["b".repeat(26)];
        let merged = merge_with_offline(&ids, vec![status(&"b".repeat(26), "dnd")]);
        assert_eq!(merged, vec![status(&"b".repeat(26), "dnd")]);
    }

    /// No ids in, nothing out — the single-user route's 404 branch depends on an empty list
    /// meaning "nothing", never on an error.
    #[test]
    fn no_ids_yields_an_empty_list() {
        assert!(merge_with_offline(&[], Vec::new()).is_empty());
    }
}
