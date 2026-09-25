//! Port of `model/report.go` — the System Console's user report and its query options.
//!
//! # Dates in the CSV are Go's `Time.String()`, not RFC 3339
//!
//! `UserReport.ToReport` renders every timestamp with `time.UnixMilli(ms).String()`, whose layout
//! is `2006-01-02 15:04:05.999999999 -0700 MST` — space-separated, local zone, fractional seconds
//! with trailing zeros trimmed, and a **zone abbreviation** at the end. That is what
//! [`go_time_string`] reproduces, the abbreviation included: the local zone is resolved to its
//! IANA name the way Go's `time.Local` is ([`local_time_zone`]) and read from `chrono_tz`'s copy
//! of the same tz database.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::{is_empty_str, is_none};
use crate::user::external::SHOW_NICKNAME_FULL_NAME;
use crate::user::{User, UserPostStats};
use crate::utils::{AppError, AppResult, get_time_for_millis};

pub const REPORT_DURATION_ALL_TIME: &str = "all_time";
pub const REPORT_DURATION_LAST_30_DAYS: &str = "last_30_days";
pub const REPORT_DURATION_PREVIOUS_MONTH: &str = "previous_month";
pub const REPORT_DURATION_LAST_6_MONTHS: &str = "last_6_months";

pub const REPORTING_MAX_PAGE_SIZE: i64 = 100;

pub const GUEST_FILTER_ALL: &str = "all";
pub const GUEST_FILTER_SINGLE_CHANNEL: &str = "single_channel";
/// The constant is `GuestFilterMultipleChannel`; the value is **`multi_channel`**.
pub const GUEST_FILTER_MULTIPLE_CHANNEL: &str = "multi_channel";

/// Port of `model.ReportExportFormats` (report.go:27) — CSV is the only one.
pub const REPORT_EXPORT_FORMATS: [&str; 1] = ["csv"];

/// Port of `model.UserReportSortColumns` (report.go:29).
///
/// **Go field names, not column names or JSON keys** — `CreateAt`, `FirstName`. A sort column is
/// rejected unless it is exactly one of these.
pub const USER_REPORT_SORT_COLUMNS: [&str; 7] = [
    "CreateAt",
    "Username",
    "FirstName",
    "LastName",
    "Nickname",
    "Email",
    "Roles",
];

/// Port of `model.AllowedGuestFilters` (report.go:31).
pub const ALLOWED_GUEST_FILTERS: [&str; 3] = [
    GUEST_FILTER_ALL,
    GUEST_FILTER_SINGLE_CHANNEL,
    GUEST_FILTER_MULTIPLE_CHANNEL,
];

/// Port of `model.IsValidReportExportFormat` (report.go:176).
pub fn is_valid_report_export_format(format: &str) -> bool {
    REPORT_EXPORT_FORMATS.contains(&format)
}

/// Go's `time.Time.String()` for an epoch-millisecond value, in the **local** zone.
///
/// Layout: `2006-01-02 15:04:05.999999999 -0700 MST`. The `.999…` verb trims trailing zeros and
/// drops the dot entirely when the fraction is zero, so a whole second renders without one.
///
/// The zone is [`local_time_zone`], so the abbreviation is the tz database's (`IST`, `UTC`,
/// `CEST`). Only when that zone cannot be named — a `/etc/localtime` that is not a link into a
/// zoneinfo tree — is the process-local offset used with no abbreviation, where Go would still
/// have one. An unrepresentable instant yields the empty string; Go cannot reach that from an
/// `int64` of milliseconds.
pub fn go_time_string(millis: i64) -> String {
    let Some(utc) = chrono::DateTime::from_timestamp_millis(millis) else {
        return String::new();
    };
    match local_time_zone() {
        Some(zone) => go_time_string_tz(&utc.with_timezone(zone)),
        None => match get_time_for_millis(millis) {
            Some(t) => go_time_string_in(&t),
            None => String::new(),
        },
    }
}

/// [`go_time_string_in`] plus the zone abbreviation, for an instant in a named zone — Go's full
/// `time.Time.String()`.
pub fn go_time_string_tz(t: &chrono::DateTime<chrono_tz::Tz>) -> String {
    format!("{} {}", go_time_string_in(t), t.format("%Z"))
}

/// The zone Go's `time.Local` resolves to (time/zoneinfo_unix.go `initLocal`), by IANA name.
///
/// `TZ` unset reads `/etc/localtime` — named here by the zoneinfo path its link points at; `TZ`
/// set but empty is **UTC**; otherwise its value, a leading `:` dropped. A name `chrono_tz` does
/// not know is `None`, and Go's own answer there is UTC — so an unknown `TZ` is UTC here too.
/// Resolved once, as Go resolves `time.Local` once.
pub fn local_time_zone() -> Option<&'static chrono_tz::Tz> {
    static ZONE: std::sync::OnceLock<Option<chrono_tz::Tz>> = std::sync::OnceLock::new();
    ZONE.get_or_init(|| {
        let name = match std::env::var("TZ") {
            Ok(tz) if tz.is_empty() => "UTC".to_owned(),
            Ok(tz) => tz.strip_prefix(':').unwrap_or(&tz).to_owned(),
            Err(_) => {
                let target = std::fs::read_link("/etc/localtime").ok()?;
                let target = target.to_string_lossy().into_owned();
                let (_, name) = target.split_once("zoneinfo/")?;
                name.to_owned()
            }
        };
        Some(name.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::Tz::UTC))
    })
    .as_ref()
}

/// The zone-explicit half of [`go_time_string`].
///
/// Split out so a test can rebuild an instant in the zone the oracle was **recorded** in rather
/// than the one the test host happens to sit in — the same reason `utils::get_start_of_day_millis`
/// takes its input as a zoned `DateTime` ([D-008]).
pub fn go_time_string_in<Tz: chrono::TimeZone>(t: &chrono::DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    // chrono writes years outside 0..=9999 with an explicit sign (`+10000`); Go does not. Only
    // the `+` differs — both use `-` for a negative year — so stripping it is the whole fix.
    let base = t
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
        .trim_start_matches('+')
        .to_string();
    let millis_part = t.timestamp_subsec_millis();
    let fraction = if millis_part == 0 {
        String::new()
    } else {
        let mut digits = format!("{millis_part:03}");
        while digits.ends_with('0') {
            digits.pop();
        }
        format!(".{digits}")
    };

    format!("{base}{fraction} {}", t.format("%z"))
}

/// Port of `model.GetReportDateRange` (report.go:47).
///
/// Three of the four durations move `start_at` only; `previous_month` is the sole case that sets
/// an **end**, bounding the window at the start of the current month. `all_time` — and any
/// unrecognised value — leaves both at zero, which the store reads as unbounded.
///
/// The month arithmetic uses the **local** zone, as Go's `time.Local` does.
pub fn get_report_date_range(date_range: &str, now: chrono::DateTime<chrono::Local>) -> (i64, i64) {
    use chrono::{Datelike, TimeZone, Timelike};

    let mut start_at = 0_i64;
    let mut end_at = 0_i64;

    if date_range == REPORT_DURATION_LAST_30_DAYS {
        start_at = (now - chrono::Duration::days(30)).timestamp_millis();
    } else if date_range == REPORT_DURATION_PREVIOUS_MONTH {
        let start_of_month = chrono::Local
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .latest();
        if let Some(start_of_month) = start_of_month {
            // `AddDate(0, -1, 0)` on the first of a month never lands in an invalid day.
            let (prev_year, prev_month) = if now.month() == 1 {
                (now.year() - 1, 12)
            } else {
                (now.year(), now.month() - 1)
            };
            if let Some(previous) = chrono::Local
                .with_ymd_and_hms(prev_year, prev_month, 1, 0, 0, 0)
                .latest()
            {
                start_at = previous.timestamp_millis();
            }
            end_at = start_of_month.timestamp_millis();
        }
    } else if date_range == REPORT_DURATION_LAST_6_MONTHS {
        // Go's `AddDate(0, -6, -0)`: six calendar months back, same day of month, normalised.
        let total = now.year() * 12 + (now.month() as i32 - 1) - 6;
        let (year, month) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
        let day = now.day();
        let candidate = chrono::Local
            .with_ymd_and_hms(year, month, day, now.hour(), now.minute(), now.second())
            .latest();
        if let Some(candidate) = candidate {
            start_at = candidate.timestamp_millis();
        }
    }

    (start_at, end_at)
}

/// Port of `model.ReportingBaseOptions` (report.go:36). No `json:` tags — query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportingBaseOptions {
    pub sort_desc: bool,
    /// Only `prev` or `next` — **not validated here**, despite the comment saying so.
    pub direction: String,
    pub page_size: i64,
    /// One of [`USER_REPORT_SORT_COLUMNS`]; checked by [`UserReportOptions::is_valid`], not by
    /// this type's.
    pub sort_column: String,
    /// The keyset cursor's value for `sort_column`.
    pub from_column_value: String,
    /// The keyset cursor's tiebreaker.
    pub from_id: String,
    pub date_range: String,
    /// Epoch milliseconds, derived from `date_range` by [`Self::populate_date_range`].
    pub start_at: i64,
    pub end_at: i64,
}

impl ReportingBaseOptions {
    /// Port of `(*ReportingBaseOptions).PopulateDateRange` (report.go:66).
    pub fn populate_date_range(&mut self, now: chrono::DateTime<chrono::Local>) {
        let (start_at, end_at) = get_report_date_range(&self.date_range, now);
        self.start_at = start_at;
        self.end_at = end_at;
    }

    /// Port of `(*ReportingBaseOptions).IsValid` (report.go:73).
    ///
    /// **One rule**, and it is guarded by `end_at > 0` — so an unbounded range with a start after
    /// "now" is valid. Note the error id has no `.app_error` suffix.
    pub fn is_valid(&self) -> AppResult {
        if self.end_at > 0 && self.start_at > self.end_at {
            return Err(Box::new(AppError::new(
                "ReportingBaseOptions.IsValid",
                "model.reporting_base_options.is_valid.bad_date_range",
                None,
                "",
                400,
            )));
        }

        Ok(())
    }
}

/// Port of `model.UserReportQuery` (report.go:81) — the raw row, before sanitisation.
///
/// No `json:` tags at all: it never reaches a client. [`UserReportQuery::to_report`] is what
/// converts it, and it **sanitises the user first**.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserReportQuery {
    pub user: User,
    pub post_stats: UserPostStats,
    pub channel_count: Option<i64>,
    /// A comma-separated list, aggregated by the query — not a slice.
    pub teams: String,
}

impl UserReportQuery {
    /// Port of `(*UserReportQuery).ToReport` (report.go:169).
    ///
    /// **Mutates the query**: `ClearNonProfileFields(true)` strips the user's non-profile fields
    /// before the report is built, so the same `UserReportQuery` cannot be reused afterwards to
    /// read them.
    pub fn to_report(&mut self) -> UserReport {
        self.user.clear_non_profile_fields(true);
        UserReport {
            user: self.user.clone(),
            post_stats: self.post_stats,
            channel_count: self.channel_count,
            teams: self.teams.clone(),
        }
    }
}

/// Port of `model.UserReport` (report.go:88) — the client-facing row.
///
/// `User` and `UserPostStats` are embedded and therefore **inlined**; only the last two fields
/// have tags of their own, and both carry `omitempty`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UserReport {
    #[serde(flatten)]
    pub user: User,

    #[serde(flatten)]
    pub post_stats: UserPostStats,

    #[serde(rename = "channel_count", skip_serializing_if = "is_none")]
    pub channel_count: Option<i64>,

    #[serde(rename = "teams", skip_serializing_if = "is_empty_str")]
    pub teams: String,
}

impl UserReport {
    /// Port of `(*UserReport).ToReport` (report.go:95) — the CSV row, 14 columns.
    ///
    /// Every nullable field renders as the **empty string** rather than `0` or `null`, and
    /// `last_login` and `delete_at` are additionally blanked when non-positive. The display name
    /// uses [`SHOW_NICKNAME_FULL_NAME`], not the caller's preference.
    pub fn to_report(&self) -> Vec<String> {
        let optional_time = |value: Option<i64>| match value {
            Some(millis) => go_time_string(millis),
            None => String::new(),
        };
        let optional_number = |value: Option<i64>| match value {
            Some(number) => number.to_string(),
            None => String::new(),
        };
        let positive_time = |millis: i64| {
            if millis > 0 {
                go_time_string(millis)
            } else {
                String::new()
            }
        };

        vec![
            self.user.id.clone(),
            self.user.username.clone(),
            self.user.email.clone(),
            go_time_string(self.user.create_at),
            self.user.get_display_name(SHOW_NICKNAME_FULL_NAME),
            self.user.roles.clone(),
            positive_time(self.user.last_login),
            optional_time(self.post_stats.last_status_at),
            optional_time(self.post_stats.last_post_date),
            optional_number(self.post_stats.days_active),
            optional_number(self.post_stats.total_posts),
            optional_number(self.channel_count),
            self.teams.clone(),
            positive_time(self.user.delete_at),
        ]
    }
}

/// Port of `model.UserReportOptions` (report.go:139).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserReportOptions {
    pub base: ReportingBaseOptions,
    pub role: String,
    pub team: String,
    pub has_no_team: bool,
    /// `hide_active` and `hide_inactive` are independent; setting both is expressible.
    pub hide_active: bool,
    pub hide_inactive: bool,
    pub search_term: String,
    /// One of [`ALLOWED_GUEST_FILTERS`], or empty for no filter.
    pub guest_filter: String,
}

impl UserReportOptions {
    /// Port of `(*UserReportOptions).IsValid` (report.go:150).
    ///
    /// The sort column is checked **unconditionally**, so an empty `sort_column` is invalid —
    /// there is no implicit default here.
    pub fn is_valid(&self) -> AppResult {
        self.base.is_valid()?;

        if !USER_REPORT_SORT_COLUMNS.contains(&self.base.sort_column.as_str()) {
            return Err(user_report_err("invalid_sort_column"));
        }

        if !self.guest_filter.is_empty()
            && !ALLOWED_GUEST_FILTERS.contains(&self.guest_filter.as_str())
        {
            return Err(user_report_err("invalid_guest_filter"));
        }

        Ok(())
    }
}

fn user_report_err(suffix: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "UserReportOptions.IsValid",
        format!("model.user_report_options.is_valid.{suffix}"),
        None,
        "",
        400,
    ))
}

#[cfg(test)]
mod go_parity {
    use super::{go_time_string_in, go_time_string_tz};
    use chrono::{FixedOffset, TimeZone};

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_go_stdlib.json"))
            .expect("behaviour_go_stdlib.json is generated by reference/dump")
    }

    /// `time.Time.String`'s layout is `2006-01-02 15:04:05.999999999 -0700 MST`. The `.999…` verb
    /// trims trailing zeros and drops the point entirely at a whole second, which is the part a
    /// reimplementation gets wrong.
    ///
    /// The instant is rebuilt in the zone the oracle was **generated** in, not the test host's:
    /// `go_time_string` reads the wall clock off the local zone, so using `Local` here would make
    /// the test pass or fail by geography ([D-008]).
    ///
    /// The whole string, the zone abbreviation included — Asia/Kolkata's history has more than
    /// `IST` in it (`HMT`, `MMT`, `+0630`), so the old instants in the corpus pin the table too.
    #[test]
    fn time_string_matches_go_with_the_zone_abbreviation() {
        let oracle = oracle();
        assert_eq!(oracle["time_zone"].as_str().unwrap(), "Asia/Kolkata");
        let zone = chrono_tz::Asia::Kolkata;

        let cases = oracle["time_string"].as_array().unwrap();
        assert!(cases.len() >= 15);
        for case in cases {
            let millis = case["millis"].as_i64().unwrap();
            let want = case["out"].as_str().unwrap();
            let instant = zone.timestamp_millis_opt(millis).single().unwrap();
            assert_eq!(
                go_time_string_tz(&instant),
                want,
                "time.UnixMilli({millis}).String()"
            );
        }
        // The offset-only half still matches with the name removed, for a caller holding a
        // fixed-offset instant.
        let fixed = FixedOffset::east_opt(5 * 3600 + 30 * 60).unwrap();
        let instant = fixed
            .timestamp_millis_opt(1_700_000_000_000)
            .single()
            .unwrap();
        assert_eq!(go_time_string_in(&instant), "2023-11-15 03:43:20 +0530");
    }
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn user_report_round_trips_the_fixture() {
        assert_fixture_round_trips!(UserReport, "user_report");
    }
}
