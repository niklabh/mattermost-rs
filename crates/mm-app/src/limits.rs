//! Port of `App.GetServerLimits` (channels/app/limits.go:24), `isAtUserLimit` (limits.go:114),
//! `shouldTrackSingleChannelGuests` (limits.go:88) and `GetLastAccessiblePostTime`
//! (app/post.go:2166).
//!
//! # Every branch reads the licence, and since 2026-09-13 every branch is answered here
//!
//! The seat limits come from `Features.Users` and `ExtraUsers` on a licence that enforces seat
//! counts, or from two constants when there is no licence at all; the post-history limit from
//! `Limits.PostHistory`; and whether single-channel guests are subtracted from the active count
//! from `IsMattermostEntry()` and a setting. Until this date the function spoke for an
//! unlicensed server only and every caller forwarded a licensed one. The licensed pair in the
//! parity harness carries an Enterprise licence that does **not** enforce seat counts, so on it
//! the answer is the four zeros and a count — which is what the seat-enforcing arms are held to
//! in the unit tests here, signed with keys minted in-process.

use mm_model::limits::ServerLimits;
use mm_model::user_count::UserCountOptions;
use mm_model::utils::{AppError, AppResult};
use mm_store::{SystemStore, UserStore};

use crate::App;

/// `maxUsersLimit` (app/limits.go:13) — the **soft** seat limit an unlicensed server enforces.
pub const MAX_USERS_LIMIT: i64 = 200;
/// `maxUsersHardLimit` (app/limits.go:14).
pub const MAX_USERS_HARD_LIMIT: i64 = 250;

impl App {
    /// Port of `App.GetServerLimits(includeUserCounts)` (limits.go:24).
    ///
    /// # Three licence shapes, three seat answers
    ///
    /// - **no licence**: the two constants, because `maxUsersLimit > 0` is always true;
    /// - **a licence that enforces seat counts** (`IsSeatCountEnforced` with `Features.Users`
    ///   set): the soft limit is `Users` and the hard limit `Users + ExtraUsers`, `ExtraUsers`
    ///   defaulting to 0;
    /// - **any other licence**: both zero, which every caller reads as "no limit".
    ///
    /// # `include_user_counts` is the expensive half
    ///
    /// The count is `Count(UserCountOptions{})` — not deleted, not remote, **bots excluded**.
    /// When `shouldTrackSingleChannelGuests` holds, the single-channel guest scan runs too and
    /// those guests come off the count (never below zero), with `Features.Users` reported as the
    /// guest limit. Go returns early **with the limits still set** when the counts are not asked
    /// for; the handler is what zeroes them for a non-admin.
    #[tracing::instrument(skip_all, fields(include_user_counts, active_user_count))]
    pub async fn get_server_limits(&self, include_user_counts: bool) -> AppResult<ServerLimits> {
        let license = self.license().await?;
        let mut limits = ServerLimits::default();

        let licensed_users = license
            .as_ref()
            .and_then(|l| l.features.as_ref())
            .and_then(|f| f.users);
        match license.as_deref() {
            None => {
                // `license == nil && maxUsersLimit > 0`.
                limits.max_users_limit = MAX_USERS_LIMIT;
                limits.max_users_hard_limit = MAX_USERS_HARD_LIMIT;
            }
            Some(l) if l.is_seat_count_enforced && licensed_users.is_some() => {
                let users = licensed_users.unwrap_or(0);
                limits.max_users_limit = users;
                limits.max_users_hard_limit = users + l.extra_users.unwrap_or(0);
            }
            Some(_) => {}
        }

        if let Some(post_history) = license
            .as_ref()
            .and_then(|l| l.limits.as_ref())
            .map(|limits| limits.post_history)
            .filter(|history| *history > 0)
        {
            limits.post_history_limit = post_history;
            limits.last_accessible_post_time = self.get_last_accessible_post_time().await?;
        }

        if !include_user_counts {
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

        if self.should_track_single_channel_guests(license.as_deref()) {
            let single_channel_guest_count = self
                .store()
                .user()
                .analytics_get_single_channel_guest_count()
                .await
                .map_err(|err| {
                    tracing::error!(error = ?err, "the single-channel guest count failed");
                    AppError::boxed(
                        "GetServerLimits",
                        "app.limits.get_app_limits.single_channel_guest_count.store_error",
                        None,
                        String::new(),
                        500,
                    )
                })?;
            // Single-channel guests are free and excluded from the primary seat count.
            limits.active_user_count = (active_user_count - single_channel_guest_count).max(0);
            limits.single_channel_guest_count = single_channel_guest_count;
            // Guests are allowed up to a 1:1 ratio with licensed seats.
            if let Some(users) = licensed_users {
                limits.single_channel_guest_limit = users;
            }
        } else {
            limits.active_user_count = active_user_count;
        }

        tracing::Span::current().record("active_user_count", limits.active_user_count);
        Ok(limits)
    }

    /// Port of `App.shouldTrackSingleChannelGuests` (limits.go:88): a licence that is not
    /// Mattermost Entry, and `GuestAccountsSettings.Enable`.
    pub(crate) fn should_track_single_channel_guests(
        &self,
        license: Option<&mm_model::license::License>,
    ) -> bool {
        match license {
            None => false,
            Some(l) if l.is_mattermost_entry() => false,
            Some(_) => self.config().guest_accounts_enable,
        }
    }

    /// Port of `App.GetPostHistoryLimit` (limits.go:104): `Limits.PostHistory`, or 0 for no
    /// licence, no limits object, or a zero.
    pub async fn get_post_history_limit(&self) -> AppResult<i64> {
        Ok(self
            .license()
            .await?
            .as_ref()
            .and_then(|l| l.limits.as_ref())
            .map(|limits| limits.post_history)
            .unwrap_or(0))
    }

    /// Port of `App.GetLastAccessiblePostTime` (app/post.go:2166).
    ///
    /// Zero — "all posts are accessible" — for no post-history limit and for a missing
    /// `Systems.LastAccessiblePostTime` row. The row is written by
    /// `ComputeLastAccessiblePostTime`, a job that is not ported, so on this side the row is
    /// whatever the Go beside us last computed; a value that does not parse as an integer is
    /// the 500 Go gives it, `common.parse_error_int64`.
    #[tracing::instrument(skip_all, fields(last_accessible_post_time))]
    pub async fn get_last_accessible_post_time(&self) -> AppResult<i64> {
        if self.get_post_history_limit().await? == 0 {
            return Ok(0);
        }
        let stored = self
            .store()
            .system()
            .get_by_name(mm_model::system::SYSTEM_LAST_ACCESSIBLE_POST_TIME)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "reading LastAccessiblePostTime failed");
                AppError::boxed(
                    "GetLastAccessiblePostTime",
                    "app.system.get_by_name.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        let Some(value) = stored else {
            return Ok(0);
        };
        let parsed = value.parse::<i64>().map_err(|_| {
            let mut params = std::collections::HashMap::new();
            params.insert("Value".to_owned(), serde_json::Value::String(value.clone()));
            AppError::boxed(
                "GetLastAccessiblePostTime",
                "common.parse_error_int64",
                Some(params),
                String::new(),
                500,
            )
        })?;
        tracing::Span::current().record("last_accessible_post_time", parsed);
        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unreachable_store() -> mm_store::SqlStore {
        mm_store::SqlStore::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .acquire_timeout(std::time::Duration::from_millis(250))
                .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
                .expect("a lazy pool is built without connecting"),
        )
    }

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

    /// The three seat shapes, with the counts left out so no store is needed: a licence that
    /// enforces seats reports `Users` and `Users + ExtraUsers`; one that does not reports zeros
    /// — **not** the unlicensed constants, which is the mistake a port would make by treating
    /// "no seat limit in the licence" as "no licence".
    #[tokio::test]
    async fn the_seat_limits_follow_the_licence_shape() {
        let enforced = crate::license::test_signing::licensed_config_from(
            r#"{"id":"mmrslicensedtestkey0000001","issued_at":1,"starts_at":1,"expires_at":4102444800000,"customer":{"id":"c","name":"n","email":"e","company":"co"},"features":{"users":10},"sku_name":"Professional","sku_short_name":"professional","is_seat_count_enforced":true,"extra_users":3}"#,
        );
        let app = crate::App::with_config(unreachable_store(), enforced);
        let limits = app.get_server_limits(false).await.expect("no count asked");
        assert_eq!(
            (limits.max_users_limit, limits.max_users_hard_limit),
            (10, 13)
        );

        let unenforced = crate::license::test_signing::licensed_config("enterprise");
        let app = crate::App::with_config(unreachable_store(), unenforced);
        let limits = app.get_server_limits(false).await.expect("no count asked");
        assert_eq!(
            (limits.max_users_limit, limits.max_users_hard_limit),
            (0, 0)
        );
        assert_eq!(limits.post_history_limit, 0);

        // Enforced without `users`: Go's second disjunct fails and the licence falls through to
        // "no limit" too.
        let no_users = crate::license::test_signing::licensed_config_from(
            r#"{"id":"mmrslicensedtestkey0000001","issued_at":1,"starts_at":1,"expires_at":4102444800000,"customer":{"id":"c","name":"n","email":"e","company":"co"},"features":{},"sku_name":"Professional","sku_short_name":"professional","is_seat_count_enforced":true}"#,
        );
        let app = crate::App::with_config(unreachable_store(), no_users);
        let limits = app.get_server_limits(false).await.expect("no count asked");
        // `Features.SetDefaults` fills `Users` with 0, which is `!= nil` in Go — so the limit
        // is an enforced zero, not "unset". Reproduced: both limits are 0 either way.
        assert_eq!(
            (limits.max_users_limit, limits.max_users_hard_limit),
            (0, 0)
        );
    }

    /// A post-history limit makes the function read `Systems.LastAccessiblePostTime`; with the
    /// store unreachable that is the 500, which proves the read is on the licensed path and only
    /// there.
    #[tokio::test]
    async fn a_post_history_limit_reads_the_last_accessible_post_time() {
        let limited = crate::license::test_signing::licensed_config_from(
            r#"{"id":"mmrslicensedtestkey0000001","issued_at":1,"starts_at":1,"expires_at":4102444800000,"customer":{"id":"c","name":"n","email":"e","company":"co"},"features":{"users":10},"sku_name":"Professional","sku_short_name":"professional","limits":{"post_history":10000}}"#,
        );
        let app = crate::App::with_config(unreachable_store(), limited);
        assert_eq!(app.get_post_history_limit().await.unwrap(), 10000);
        let err = app
            .get_server_limits(false)
            .await
            .expect_err("the system row is read, and the store is unreachable");
        assert_eq!(err.id, "app.system.get_by_name.app_error");

        let unlimited = crate::license::test_signing::licensed_config("enterprise");
        let app = crate::App::with_config(unreachable_store(), unlimited);
        assert_eq!(app.get_last_accessible_post_time().await.unwrap(), 0);
    }

    /// `shouldTrackSingleChannelGuests`: no licence, or an Entry licence, or the setting off —
    /// each alone is false; only a non-Entry licence with guests enabled is true.
    #[tokio::test]
    async fn single_channel_guests_are_tracked_only_with_a_non_entry_licence_and_guests_on() {
        let mut config = crate::license::test_signing::licensed_config("enterprise");
        config.guest_accounts_enable = true;
        let app = crate::App::with_config(unreachable_store(), config);
        let license = app.license().await.unwrap();
        assert!(app.should_track_single_channel_guests(license.as_deref()));
        assert!(!app.should_track_single_channel_guests(None));

        let mut entry = crate::license::test_signing::licensed_config("entry");
        entry.guest_accounts_enable = true;
        let app = crate::App::with_config(unreachable_store(), entry);
        let license = app.license().await.unwrap();
        assert!(!app.should_track_single_channel_guests(license.as_deref()));

        let off = crate::license::test_signing::licensed_config("enterprise");
        let app = crate::App::with_config(unreachable_store(), off);
        let license = app.license().await.unwrap();
        assert!(!app.should_track_single_channel_guests(license.as_deref()));
    }
}
