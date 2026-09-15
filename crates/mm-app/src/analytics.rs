//! The two period constants of `channels/app/analytics.go` (`DayMilliseconds`,
//! `MonthMilliseconds`). `getLicenseLoadMetric` measures monthly active users over exactly this
//! window — 31 days, not a calendar month.

/// `app.DayMilliseconds` (analytics.go:17).
pub const DAY_MILLISECONDS: i64 = 24 * 60 * 60 * 1000;
/// `app.MonthMilliseconds` (analytics.go:18) — `31 * DayMilliseconds`.
pub const MONTH_MILLISECONDS: i64 = 31 * DAY_MILLISECONDS;

use chrono::{Datelike, Days, Local, NaiveDate, TimeZone};
use mm_model::analytics_row::{AnalyticsRow, AnalyticsRows};
use mm_model::team_search::TeamSearch;
use mm_model::user_count::UserCountOptions;
use mm_model::utils::{AppError, AppResult};
use mm_store::{
    ChannelStore, CommandStore, FileInfoStore, PostStore, SessionStore, StoreError, TeamStore,
    UserStore, WebhookStore,
};

use crate::App;

/// The five reports `getAnalytics` (analytics.go:42) dispatches on; anything else is Go's
/// `nil, nil`, which the handler turns into a 400 naming `name`.
const ANALYTICS_STANDARD: &str = "standard";
const ANALYTICS_BOT_POST_COUNTS_DAY: &str = "bot_post_counts_day";
const ANALYTICS_POST_COUNTS_DAY: &str = "post_counts_day";
const ANALYTICS_USER_COUNTS_WITH_POSTS_DAY: &str = "user_counts_with_posts_day";
const ANALYTICS_EXTRA_COUNTS: &str = "extra_counts";

/// `model.NewAppError("GetAnalytics", id, nil, "", 500).Wrap(err)` — the wrapped store error is
/// logged rather than carried, since `detailed_error` is wiped on the wire either way.
fn analytics_error(id: &str, err: StoreError) -> Box<AppError> {
    tracing::error!(error = %err, id, "analytics query failed");
    AppError::boxed("GetAnalytics", id, None, String::new(), 500)
}

fn row(name: &str, value: f64) -> AnalyticsRow {
    AnalyticsRow {
        name: name.to_owned(),
        value,
    }
}

/// `utils.Yesterday()` — `time.Now().AddDate(0, 0, -1)` in the process's local zone, which is
/// a calendar day and not 24 hours (the two differ across a DST change).
fn yesterday() -> chrono::DateTime<Local> {
    let now = Local::now();
    now.checked_sub_days(Days::new(1)).unwrap_or(now)
}

/// `t.AddDate(0, 0, -days)` on the date alone.
fn days_before(date: NaiveDate, days: u64) -> NaiveDate {
    date.checked_sub_days(Days::new(days)).unwrap_or(date)
}

/// `utils.MillisFromTime(utils.StartOfDay(t))` for a local date: midnight in the local zone.
fn start_of_day_millis(date: NaiveDate) -> i64 {
    Local
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .map(|t| t.timestamp_millis())
        .unwrap_or_default()
}

/// `utils.MillisFromTime(utils.EndOfDay(t))`: 23:59:59.999999999 local, whose millisecond
/// floor is `.999`.
fn end_of_day_millis(date: NaiveDate) -> i64 {
    Local
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 23, 59, 59)
        .single()
        .map(|t| t.timestamp_millis() + 999)
        .unwrap_or_default()
}

impl App {
    /// Port of `App.GetAnalytics` (analytics.go:21) and the private `getAnalytics` behind it,
    /// with `forSupportPacket == false`.
    ///
    /// `Ok(None)` is Go's `nil, nil` for a `name` that is none of the five reports. The system
    /// user count is read **first, whatever the report**, so a broken `Users` table is the
    /// `get_total_users_count` 500 even for a report that never uses it — and it is what decides
    /// `skipIntensiveQueries` against `AnalyticsSettings.MaxUsersForStatistics`, which only the
    /// `user_counts_with_posts_day` report honours.
    #[tracing::instrument(skip(self), fields(rows))]
    pub async fn get_analytics(
        &self,
        name: &str,
        team_id: &str,
    ) -> AppResult<Option<AnalyticsRows>> {
        let system_user_count = self
            .store()
            .user()
            .count(&UserCountOptions::default())
            .await
            .map_err(|err| analytics_error("app.user.get_total_users_count.app_error", err))?;

        let skip_intensive_queries = system_user_count > self.config().max_users_for_statistics;
        if skip_intensive_queries {
            tracing::warn!(
                limit = self.config().max_users_for_statistics,
                "Number of users in the system is higher than the configured limit. Skipping intensive SQL queries."
            );
        }

        let rows = match name {
            ANALYTICS_STANDARD => {
                self.get_standard_analytics(team_id, system_user_count)
                    .await?
            }
            ANALYTICS_BOT_POST_COUNTS_DAY => self.get_post_counts_analytics(team_id, true).await?,
            ANALYTICS_POST_COUNTS_DAY => self.get_post_counts_analytics(team_id, false).await?,
            ANALYTICS_USER_COUNTS_WITH_POSTS_DAY => {
                self.get_user_counts_with_posts_analytics(team_id, skip_intensive_queries)
                    .await?
            }
            ANALYTICS_EXTRA_COUNTS => self.get_extra_counts_analytics(team_id).await?,
            _ => return Ok(None),
        };
        tracing::Span::current().record("rows", rows.0.len());
        Ok(Some(rows))
    }

    /// Port of `App.getStandardAnalytics` (analytics.go:63): twelve rows in a fixed order.
    ///
    /// # Three rows are this process's and never the pair's
    ///
    /// `total_websocket_connections` is this hub's count, `total_master_db_connections` this
    /// pool's open connections and `total_read_db_connections` its (absent) replicas — each
    /// server reports itself, as Go's cluster branch would sum its own over its peers. The
    /// cluster branch itself (`a.Cluster() != nil && ClusterSettings.Enable`) is not taken:
    /// there is no cluster interface here, and the sum over no peers is the local figure.
    ///
    /// # With a team, two rows change meaning
    ///
    /// `unique_user_count` becomes the team's member count and `inactive_user_count` is the
    /// literal `-1` — the team branch never runs the inactive query. `team_count` stays the
    /// whole server's live teams either way.
    ///
    /// Go runs the queries under an `errgroup` of two and reports whichever failed first; run
    /// in sequence here they report the first in Go's launch order, which is the only order a
    /// test can pin.
    async fn get_standard_analytics(
        &self,
        team_id: &str,
        system_user_count: i64,
    ) -> AppResult<AnalyticsRows> {
        let store = self.store();

        let channel_counts = store
            .channel()
            .analytics_count_all(team_id)
            .await
            .map_err(|err| analytics_error("app.channel.analytics_type_count.app_error", err))?;

        let (users_count, inactive_users_count) = if team_id.is_empty() {
            let inactive = store
                .user()
                .analytics_get_inactive_users_count()
                .await
                .map_err(|err| {
                    analytics_error("app.user.analytics_get_inactive_users_count.app_error", err)
                })?;
            (0, inactive)
        } else {
            let users = store
                .user()
                .count(&UserCountOptions {
                    team_id: team_id.to_owned(),
                    ..UserCountOptions::default()
                })
                .await
                .map_err(|err| analytics_error("app.user.get_total_users_count.app_error", err))?;
            (users, 0)
        };

        let posts_count = store
            .post()
            .analytics_post_count_by_team(team_id)
            .await
            .map_err(|err| analytics_error("app.post.analytics_posts_count.app_error", err))?;

        // `AnalyticsTeamCount(nil)`: with no options Go adds `DeleteAt = 0`, which the port's
        // options spell as an explicit `include_deleted: Some(false)`.
        let teams_count = store
            .team()
            .analytics_team_count(&TeamSearch {
                include_deleted: Some(false),
                ..TeamSearch::default()
            })
            .await
            .map_err(|err| analytics_error("app.team.analytics_team_count.app_error", err))?;

        let active_options = UserCountOptions {
            include_bot_accounts: false,
            include_deleted: false,
            ..UserCountOptions::default()
        };
        let daily_active_users_count = store
            .user()
            .analytics_active_count(DAY_MILLISECONDS, &active_options)
            .await
            .map_err(|err| {
                analytics_error("app.user.analytics_daily_active_users.app_error", err)
            })?;
        // The same id as the daily row's: Go reuses `analytics_daily_active_users` for the
        // monthly query too.
        let monthly_active_users_count = store
            .user()
            .analytics_active_count(MONTH_MILLISECONDS, &active_options)
            .await
            .map_err(|err| {
                analytics_error("app.user.analytics_daily_active_users.app_error", err)
            })?;

        let license = self.license().await?;
        let single_channel_guest_count =
            if self.should_track_single_channel_guests(license.as_deref()) {
                store
                    .user()
                    .analytics_get_single_channel_guest_count()
                    .await
                    .map_err(|err| {
                        analytics_error(
                            "app.user.analytics_get_single_channel_guest_count.app_error",
                            err,
                        )
                    })?
            } else {
                0
            };

        let count_of = |channel_type: &str| -> f64 {
            channel_counts.get(channel_type).copied().unwrap_or(0) as f64
        };
        let (unique_user_count, inactive_user_count) = if team_id.is_empty() {
            (system_user_count as f64, inactive_users_count as f64)
        } else {
            (users_count as f64, -1.0)
        };

        Ok(AnalyticsRows(vec![
            row(
                "channel_open_count",
                count_of(mm_model::channel::CHANNEL_TYPE_OPEN),
            ),
            row(
                "channel_private_count",
                count_of(mm_model::channel::CHANNEL_TYPE_PRIVATE),
            ),
            row("post_count", posts_count as f64),
            row("unique_user_count", unique_user_count),
            row("team_count", teams_count as f64),
            row(
                "total_websocket_connections",
                self.hub().conn_count() as f64,
            ),
            row(
                "total_master_db_connections",
                store.total_master_db_connections() as f64,
            ),
            row(
                "total_read_db_connections",
                store.total_read_db_connections() as f64,
            ),
            row("daily_active_users", daily_active_users_count as f64),
            row("monthly_active_users", monthly_active_users_count as f64),
            row("inactive_user_count", inactive_user_count),
            row(
                "single_channel_guest_count",
                single_channel_guest_count as f64,
            ),
        ]))
    }

    /// Port of `App.getPostCountsAnalytics` and `getBotPostCountsAnalytics` (analytics.go:227,
    /// :215), which differ only in `BotsOnly`, plus the date arithmetic of
    /// `SqlPostStore.AnalyticsPostCountsByDay` (post_store.go:2469): yesterday, and the day
    /// thirty-one before it, as `YYYY-MM-DD` in this process's local zone.
    async fn get_post_counts_analytics(
        &self,
        team_id: &str,
        bots_only: bool,
    ) -> AppResult<AnalyticsRows> {
        let end_day = yesterday().date_naive();
        let start_day = days_before(end_day, 31);
        self.store()
            .post()
            .analytics_post_counts_by_day(team_id, bots_only, start_day, end_day)
            .await
            .map_err(|err| analytics_error("app.post.analytics_posts_count_by_day.app_error", err))
    }

    /// Port of `App.getUserCountsWithPostsAnalytics` (analytics.go:239): the one report the
    /// user-count cap applies to, whose skipped form is a single row named `""` with `-1`.
    /// The window is `StartOfDay(yesterday - 31d)` to `EndOfDay(yesterday)`, local zone, in
    /// milliseconds.
    async fn get_user_counts_with_posts_analytics(
        &self,
        team_id: &str,
        skip_intensive_queries: bool,
    ) -> AppResult<AnalyticsRows> {
        if skip_intensive_queries {
            return Ok(AnalyticsRows(vec![row("", -1.0)]));
        }
        let end_day = yesterday().date_naive();
        let start_day = days_before(end_day, 31);
        self.store()
            .post()
            .analytics_user_counts_with_posts_by_day(
                team_id,
                start_of_day_millis(start_day),
                end_of_day_millis(end_day),
            )
            .await
            .map_err(|err| {
                analytics_error("app.post.analytics_user_counts_posts_by_day.app_error", err)
            })
    }

    /// Port of `App.getExtraCountsAnalytics` (analytics.go:254): six rows. With a team this is
    /// a 500 on both servers — see `WebhookStore::analytics_outgoing_count`.
    async fn get_extra_counts_analytics(&self, team_id: &str) -> AppResult<AnalyticsRows> {
        let store = self.store();
        let incoming_webhook_count = store
            .webhook()
            .analytics_incoming_count(team_id, "")
            .await
            .map_err(|err| {
                analytics_error("app.webhooks.analytics_incoming_count.app_error", err)
            })?;
        let outgoing_webhook_count = store
            .webhook()
            .analytics_outgoing_count(team_id)
            .await
            .map_err(|err| {
                analytics_error("app.webhooks.analytics_outgoing_count.app_error", err)
            })?;
        let commands_count = store
            .command()
            .analytics_command_count(team_id)
            .await
            .map_err(|err| analytics_error("app.analytics.getanalytics.internal_error", err))?;
        let sessions_count = store
            .session()
            .analytics_session_count()
            .await
            .map_err(|err| analytics_error("app.session.analytics_session_count.app_error", err))?;
        let file_count = store
            .file_info()
            .count_all()
            .await
            .map_err(|err| analytics_error("app.file_info.get_count.app_error", err))?;
        let file_size = store
            .file_info()
            .get_storage_usage(false)
            .await
            .map_err(|err| analytics_error("app.file_info.get_storage_usage.app_error", err))?;

        Ok(AnalyticsRows(vec![
            row("incoming_webhook_count", incoming_webhook_count as f64),
            row("outgoing_webhook_count", outgoing_webhook_count as f64),
            row("command_count", commands_count as f64),
            row("session_count", sessions_count as f64),
            row("total_file_count", file_count as f64),
            row("total_file_size", file_size as f64),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `EndOfDay` is `.999999999`, whose millisecond value ends `999`; `StartOfDay` is the
    /// exact midnight. Both in the local zone, so the difference is a whole day less one ms.
    #[test]
    fn the_day_bounds_span_one_day_less_a_millisecond() {
        let day = NaiveDate::from_ymd_opt(2024, 3, 15).unwrap();
        let start = start_of_day_millis(day);
        let end = end_of_day_millis(day);
        assert_eq!(end - start, 24 * 60 * 60 * 1000 - 1);
        assert_eq!(end % 1000, 999);
        assert_eq!(
            start_of_day_millis(days_before(day, 31)),
            start - 31 * DAY_MILLISECONDS
        );
    }

    /// `AddDate(0, 0, -31)` on the date, not 31 × 24 h on the instant.
    #[test]
    fn days_before_is_calendar_arithmetic() {
        let day = NaiveDate::from_ymd_opt(2024, 3, 1).unwrap();
        assert_eq!(
            days_before(day, 31),
            NaiveDate::from_ymd_opt(2024, 1, 30).unwrap()
        );
    }
}
