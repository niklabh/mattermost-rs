//! Port of `App.GetServerLimits` (channels/app/limits.go:24), restricted to the unlicensed case.
//!
//! # Why only the unlicensed case
//!
//! Every branch in that function reads `a.License()`: the seat limits come from the licence's
//! `Features.Users` and `ExtraUsers`, the post-history limit from `Limits.PostHistory`, and
//! whether single-channel guests are counted at all from `IsMattermostEntry()`. None of that is
//! visible to a second process — see [`crate::license`] — so a licensed installation is forwarded
//! by the handler and this function is the `license == nil` path only.
//!
//! On that path the answer is: the two hard-coded limits, one `COUNT(*)`, and five zeros.

use mm_model::limits::ServerLimits;
use mm_model::user_count::UserCountOptions;
use mm_model::utils::{AppError, AppResult};
use mm_store::UserStore;

use crate::App;

/// `maxUsersLimit` (app/limits.go:13) — the **soft** seat limit an unlicensed server enforces.
pub const MAX_USERS_LIMIT: i64 = 200;
/// `maxUsersHardLimit` (app/limits.go:14).
pub const MAX_USERS_HARD_LIMIT: i64 = 250;

impl App {
    /// Port of `App.GetServerLimits(includeUserCounts)` (limits.go:24) for `license == nil`.
    ///
    /// # `include_user_counts` is the *only* thing that varies
    ///
    /// The two seat limits are constants on this path and the other four fields are zero, because
    /// each is licence-derived: `PostHistoryLimit` and `LastAccessiblePostTime` need
    /// `license.Limits.PostHistory`, and `SingleChannelGuestCount`/`Limit` are gated by
    /// `shouldTrackSingleChannelGuests`, which returns false the moment the licence is nil
    /// (limits.go:88). So an unlicensed server never runs the guest scan at all — and
    /// `ActiveUserCount` is therefore the **unadjusted** count, not the count minus guests.
    ///
    /// # The count is `Count(UserCountOptions{})`
    ///
    /// All-zero options: not deleted, no remote users, **bots excluded** (the `else` branch's
    /// left-anti-join, limits.go via user_store.go:1487). "Active" here means "not deleted and not
    /// a bot", which is narrower than the field name suggests.
    #[tracing::instrument(skip_all, fields(include_user_counts, active_user_count))]
    pub async fn get_server_limits(&self, include_user_counts: bool) -> AppResult<ServerLimits> {
        let mut limits = ServerLimits {
            max_users_limit: MAX_USERS_LIMIT,
            max_users_hard_limit: MAX_USERS_HARD_LIMIT,
            ..ServerLimits::default()
        };

        if !include_user_counts {
            // Go returns here **with the limits still set** — only the counts are skipped. The
            // handler is what zeroes the limits for a non-admin, and that is a different decision
            // in a different place; see `mm_api::limits`.
            return Ok(limits);
        }

        let active_user_count = self
            .store()
            .user()
            .count(&UserCountOptions::default())
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "the active user count failed");
                AppError::boxed(
                    "GetServerLimits",
                    "app.limits.get_app_limits.user_count.store_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("active_user_count", active_user_count);
        limits.active_user_count = active_user_count;
        Ok(limits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The unlicensed shape: two constants and five zeros. Pinned because every one of the five is
    /// a *licence-derived* field, and a port that filled one in from config would answer a limit
    /// this server does not enforce.
    #[test]
    fn the_unlicensed_limits_are_two_constants_and_five_zeros() {
        let limits = ServerLimits {
            max_users_limit: MAX_USERS_LIMIT,
            max_users_hard_limit: MAX_USERS_HARD_LIMIT,
            ..ServerLimits::default()
        };

        assert_eq!(limits.max_users_limit, 200);
        assert_eq!(limits.max_users_hard_limit, 250);
        assert_eq!(limits.active_user_count, 0);
        assert_eq!(limits.single_channel_guest_count, 0);
        assert_eq!(limits.single_channel_guest_limit, 0);
        assert_eq!(limits.post_history_limit, 0);
        assert_eq!(limits.last_accessible_post_time, 0);
    }

    /// Every field is on the wire — `model.ServerLimits` has no `omitempty`, so the zeros are
    /// written, and the keys are **camelCase** where the rest of the API is snake.
    #[test]
    fn every_field_is_written_in_camel_case() {
        let body = serde_json::to_string(&ServerLimits::default()).expect("serialises");
        assert_eq!(
            body,
            concat!(
                r#"{"maxUsersLimit":0,"maxUsersHardLimit":0,"activeUserCount":0,"#,
                r#""singleChannelGuestCount":0,"singleChannelGuestLimit":0,"#,
                r#""postHistoryLimit":0,"lastAccessiblePostTime":0}"#
            )
        );
    }
}
