//! The two period constants of `channels/app/analytics.go` (`DayMilliseconds`,
//! `MonthMilliseconds`). `getLicenseLoadMetric` measures monthly active users over exactly this
//! window — 31 days, not a calendar month.

/// `app.DayMilliseconds` (analytics.go:17).
pub const DAY_MILLISECONDS: i64 = 24 * 60 * 60 * 1000;
/// `app.MonthMilliseconds` (analytics.go:18) — `31 * DayMilliseconds`.
pub const MONTH_MILLISECONDS: i64 = 31 * DAY_MILLISECONDS;
