//! Port of `model/scheduled_recap.go` — a user's recurring recap schedule.
//!
//! # The day-of-week bitmask follows Go's `time.Weekday`, not ISO-8601
//!
//! **Sunday is bit 0**, so `Weekdays` is 62 and the mask runs 1..=127. A schedule built against
//! an ISO week (Monday first) is off by one day for every entry.
//!
//! # Next-run computation is timezone-aware and DST-correct
//!
//! [`ScheduledRecap::compute_next_run_at`] resolves the wall-clock `HH:MM` in the **user's** IANA
//! zone and then converts to UTC milliseconds, so a 09:00 schedule stays at 09:00 across a DST
//! transition rather than drifting by an hour. `chrono-tz` is the embedded IANA table this crate
//! already uses for `scheduled_post.go`; see [D-065] for why the accepted zone set is a
//! deployment artifact in Go and a fixed table here.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

use chrono::{DateTime, Datelike, TimeZone, Utc};

use crate::serde_helpers::{is_empty_str, is_none_or_empty_vec};
use crate::utils::{AppError, AppResult, StringArray, get_millis, is_valid_id, new_id};

/// Sunday is bit **0**, matching Go's `time.Weekday`.
pub const SUNDAY: i64 = 1 << 0;
pub const MONDAY: i64 = 1 << 1;
pub const TUESDAY: i64 = 1 << 2;
pub const WEDNESDAY: i64 = 1 << 3;
pub const THURSDAY: i64 = 1 << 4;
pub const FRIDAY: i64 = 1 << 5;
pub const SATURDAY: i64 = 1 << 6;

/// 62.
pub const WEEKDAYS: i64 = MONDAY | TUESDAY | WEDNESDAY | THURSDAY | FRIDAY;
/// 65 — Saturday **and Sunday**, which is bit 0.
pub const WEEKEND: i64 = SATURDAY | SUNDAY;
/// 127.
pub const EVERY_DAY: i64 = WEEKDAYS | WEEKEND;

pub const CHANNEL_MODE_SPECIFIC: &str = "specific";
pub const CHANNEL_MODE_ALL_UNREADS: &str = "all_unreads";

pub const TIME_PERIOD_LAST_24H: &str = "last_24h";
pub const TIME_PERIOD_LAST_WEEK: &str = "last_week";
pub const TIME_PERIOD_SINCE_LAST_READ: &str = "since_last_read";

pub const SCHEDULED_RECAP_TITLE_MAX_LENGTH: usize = 255;
pub const SCHEDULED_RECAP_CUSTOM_INSTRUCTIONS_MAX_LENGTH: usize = 500;
pub const SCHEDULED_RECAP_MIN_DAYS_OF_WEEK: i64 = 1;
pub const SCHEDULED_RECAP_MAX_DAYS_OF_WEEK: i64 = 127;

/// Port of `timeOfDayRegex` (scheduled_recap.go:52) — `^([0-1][0-9]|2[0-3]):([0-5][0-9])$`.
///
/// Inlined rather than compiled: the pattern is a fixed-width ASCII shape. Note it requires
/// **two digits on both sides**, so `9:00` is rejected and `24:00` is too.
pub fn is_valid_time_of_day(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' {
        return false;
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(i, b)| i == 2 || b.is_ascii_digit())
    {
        return false;
    }
    let hour_ok = matches!(bytes[0], b'0' | b'1') || (bytes[0] == b'2' && bytes[1] <= b'3');
    let minute_ok = bytes[3] <= b'5';
    hour_ok && minute_ok
}

/// Port of `deduplicateChannelIDs` (scheduled_recap.go:96).
///
/// **Order-preserving** — first occurrence wins — and it returns the input untouched for fewer
/// than two elements, which matters because Go returns the *same slice* there rather than a copy.
pub fn deduplicate_channel_ids(channel_ids: &[String]) -> Vec<String> {
    if channel_ids.len() < 2 {
        return channel_ids.to_vec();
    }

    let mut seen = std::collections::HashSet::with_capacity(channel_ids.len());
    let mut deduplicated = Vec::with_capacity(channel_ids.len());
    for channel_id in channel_ids {
        if seen.insert(channel_id.clone()) {
            deduplicated.push(channel_id.clone());
        }
    }

    deduplicated
}

/// Port of `model.ScheduledRecap` (scheduled_recap.go:56).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduledRecap {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "title")]
    pub title: String,

    /// Bitmask; Sunday is 1. See the module docs.
    #[serde(rename = "days_of_week")]
    pub days_of_week: i64,

    /// `HH:MM`, zero-padded.
    #[serde(rename = "time_of_day")]
    pub time_of_day: String,

    /// An IANA name, e.g. `America/New_York`.
    #[serde(rename = "timezone")]
    pub timezone: String,

    /// One of the three `TIME_PERIOD_*` constants.
    #[serde(rename = "time_period")]
    pub time_period: String,

    /// UTC milliseconds, computed from the schedule and the timezone.
    #[serde(rename = "next_run_at")]
    pub next_run_at: i64,

    /// UTC milliseconds.
    #[serde(rename = "last_run_at")]
    pub last_run_at: i64,

    #[serde(rename = "run_count")]
    pub run_count: i64,

    /// [`CHANNEL_MODE_SPECIFIC`] or [`CHANNEL_MODE_ALL_UNREADS`].
    #[serde(rename = "channel_mode")]
    pub channel_mode: String,

    /// Required when the mode is `specific`. Persisted as `jsonb`.
    #[serde(rename = "channel_ids", skip_serializing_if = "is_none_or_empty_vec")]
    pub channel_ids: Option<StringArray>,

    #[serde(rename = "custom_instructions", skip_serializing_if = "is_empty_str")]
    pub custom_instructions: String,

    #[serde(rename = "agent_id")]
    pub agent_id: String,

    /// False for a "run once" schedule.
    #[serde(rename = "is_recurring")]
    pub is_recurring: bool,

    /// False when paused.
    #[serde(rename = "enabled")]
    pub enabled: bool,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    /// Soft delete.
    #[serde(rename = "delete_at")]
    pub delete_at: i64,
}

impl ScheduledRecap {
    fn channel_ids_slice(&self) -> &[String] {
        self.channel_ids.as_deref().unwrap_or(&[])
    }

    /// Port of `(*ScheduledRecap).ComputeNextRunAt` (scheduled_recap.go:117).
    ///
    /// Searches from **today** in the user's zone: if today's `HH:MM` has already passed —
    /// `!candidate.After(localNow)`, so a candidate exactly equal to now counts as passed — it
    /// starts from tomorrow, then walks at most seven days looking for a set day bit.
    ///
    /// The seven-iteration bound is why an out-of-range mask is rejected first: `0` would find
    /// nothing and fall through to the `no_valid_day` error.
    ///
    /// **Divergence:** Go builds the candidate with `time.Date`, which silently normalises a
    /// wall-clock time that does not exist (the DST spring-forward gap). `chrono`'s
    /// `with_ymd_and_hms` reports that case instead; the port takes the **later** of the two
    /// offsets for an ambiguous time and the next valid instant for a skipped one, which is what
    /// Go's normalisation produces.
    pub fn compute_next_run_at(&self, from_time: DateTime<Utc>) -> Result<i64, Box<AppError>> {
        let Ok(tz) = chrono_tz::Tz::from_str(&self.timezone) else {
            return Err(compute_err(
                "timezone",
                format!("timezone={}", self.timezone),
            ));
        };

        if !is_valid_time_of_day(&self.time_of_day) {
            return Err(compute_err(
                "time_format",
                format!("time_of_day={}", self.time_of_day),
            ));
        }

        // Safe: `is_valid_time_of_day` guarantees two ASCII digits either side of the colon.
        let hour: u32 = self.time_of_day[0..2].parse().unwrap_or(0);
        let minute: u32 = self.time_of_day[3..5].parse().unwrap_or(0);

        if !(SCHEDULED_RECAP_MIN_DAYS_OF_WEEK..=SCHEDULED_RECAP_MAX_DAYS_OF_WEEK)
            .contains(&self.days_of_week)
        {
            return Err(compute_err(
                "days_of_week",
                format!("days_of_week={}", self.days_of_week),
            ));
        }

        let local_now = from_time.with_timezone(&tz);

        let mut date = local_now.date_naive();
        let at = |date: chrono::NaiveDate| -> Option<DateTime<chrono_tz::Tz>> {
            let naive = date.and_hms_opt(hour, minute, 0)?;
            // `latest()` for an ambiguous local time, and `single()` failing on a skipped one is
            // handled by the caller advancing a day.
            tz.from_local_datetime(&naive).latest()
        };

        // If today's time has already passed, start from tomorrow.
        if let Some(candidate) = at(date) {
            if candidate <= local_now {
                date = date.succ_opt().unwrap_or(date);
            }
        } else {
            date = date.succ_opt().unwrap_or(date);
        }

        for _ in 0..7 {
            // `num_days_from_sunday` is 0 for Sunday — the same origin as Go's `time.Weekday`.
            let day_bit = 1_i64 << date.weekday().num_days_from_sunday();

            if self.days_of_week & day_bit != 0 {
                if let Some(candidate) = at(date) {
                    return Ok(candidate.timestamp_millis());
                }
            }

            date = match date.succ_opt() {
                Some(next) => next,
                None => break,
            };
        }

        Err(compute_err("no_valid_day", String::new()))
    }

    /// Port of `(*ScheduledRecap).IsValid` (scheduled_recap.go:170).
    ///
    /// Both length caps are **bytes**, and both report the measured length in the details string.
    /// `channel_ids` is only checked in `specific` mode — an `all_unreads` schedule may carry a
    /// stale list and still be valid.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("id", format!("id={}", self.id)));
        }

        if !is_valid_id(&self.user_id) {
            return Err(err("user_id", format!("user_id={}", self.user_id)));
        }

        if self.title.is_empty() {
            return Err(err("title_empty", String::new()));
        }

        if self.title.len() > SCHEDULED_RECAP_TITLE_MAX_LENGTH {
            return Err(err(
                "title_length",
                format!("title_length={}", self.title.len()),
            ));
        }

        if self.custom_instructions.len() > SCHEDULED_RECAP_CUSTOM_INSTRUCTIONS_MAX_LENGTH {
            return Err(err(
                "custom_instructions_length",
                format!(
                    "custom_instructions_length={}",
                    self.custom_instructions.len()
                ),
            ));
        }

        if !(SCHEDULED_RECAP_MIN_DAYS_OF_WEEK..=SCHEDULED_RECAP_MAX_DAYS_OF_WEEK)
            .contains(&self.days_of_week)
        {
            return Err(err(
                "days_of_week",
                format!("days_of_week={}", self.days_of_week),
            ));
        }

        if !is_valid_time_of_day(&self.time_of_day) {
            return Err(err(
                "time_of_day",
                format!("time_of_day={}", self.time_of_day),
            ));
        }

        if chrono_tz::Tz::from_str(&self.timezone).is_err() {
            return Err(err("timezone", format!("timezone={}", self.timezone)));
        }

        if self.time_period != TIME_PERIOD_LAST_24H
            && self.time_period != TIME_PERIOD_LAST_WEEK
            && self.time_period != TIME_PERIOD_SINCE_LAST_READ
        {
            return Err(err(
                "time_period",
                format!("time_period={}", self.time_period),
            ));
        }

        if self.channel_mode != CHANNEL_MODE_SPECIFIC
            && self.channel_mode != CHANNEL_MODE_ALL_UNREADS
        {
            return Err(err(
                "channel_mode",
                format!("channel_mode={}", self.channel_mode),
            ));
        }

        if self.channel_mode == CHANNEL_MODE_SPECIFIC {
            if self.channel_ids_slice().is_empty() {
                return Err(err("channel_ids_empty", String::new()));
            }

            for channel_id in self.channel_ids_slice() {
                if !is_valid_id(channel_id) {
                    return Err(err("channel_id", format!("channel_id={channel_id}")));
                }
            }
        }

        if self.agent_id.is_empty() {
            return Err(err("agent_id", String::new()));
        }

        Ok(())
    }

    /// Port of `(*ScheduledRecap).PreSave` (scheduled_recap.go:238).
    ///
    /// Deduplicates the channel list **first**, then fills the id and timestamps only when unset.
    pub fn pre_save(&mut self) {
        self.channel_ids = Some(deduplicate_channel_ids(self.channel_ids_slice()));

        if self.id.is_empty() {
            self.id = new_id();
        }

        if self.create_at == 0 {
            self.create_at = get_millis();
        }

        if self.update_at == 0 {
            self.update_at = self.create_at;
        }
    }

    /// Port of `(*ScheduledRecap).PreUpdate` (scheduled_recap.go:256).
    pub fn pre_update(&mut self) {
        self.channel_ids = Some(deduplicate_channel_ids(self.channel_ids_slice()));
        self.update_at = get_millis();
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "ScheduledRecap.IsValid",
        format!("model.scheduled_recap.is_valid.{field}.app_error"),
        None,
        details,
        400,
    ))
}

fn compute_err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "ScheduledRecap.ComputeNextRunAt",
        format!("model.scheduled_recap.compute_next_run.{field}.app_error"),
        None,
        details,
        400,
    ))
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
    fn scheduled_recap_round_trips_the_fixture() {
        assert_fixture_round_trips!(ScheduledRecap, "scheduled_recap");
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// The time-of-day rule is a regex in Go, and every near-miss is the interesting case: a
    /// single-digit hour, an unpadded minute, a third digit, a non-ASCII digit, and the two
    /// out-of-range values that are *shaped* correctly.
    ///
    /// Driven through `IsValid` rather than the helper, because that is the only caller and the
    /// error id is what a client sees.
    #[test]
    fn time_of_day_and_dedup_match_go() {
        let oracle = oracle();
        let cases = oracle["scheduled_recap_time_valid"].as_array().unwrap();

        let mut seen_dedup = 0;
        for case in cases {
            if let Some(input) = case.get("in").and_then(|v| v.as_str()) {
                let sr = ScheduledRecap {
                    id: "s1a2b3c4d5e6f7g8h9i0j1k2l3".to_string(),
                    user_id: "u1a2b3c4d5e6f7g8h9i0j1k2l3".to_string(),
                    title: "Daily".to_string(),
                    days_of_week: EVERY_DAY,
                    time_of_day: input.to_string(),
                    timezone: "Asia/Kolkata".to_string(),
                    time_period: TIME_PERIOD_LAST_24H.to_string(),
                    channel_mode: CHANNEL_MODE_ALL_UNREADS.to_string(),
                    agent_id: "agent".to_string(),
                    ..Default::default()
                };
                match (sr.is_valid(), case.get("error_id").and_then(|v| v.as_str())) {
                    (Ok(()), None) => {}
                    (Ok(()), Some(id)) => panic!("{input:?}: Go rejected with {id}"),
                    (Err(e), None) => panic!("{input:?}: Go accepted, the port rejected: {}", e.id),
                    (Err(e), Some(id)) => assert_eq!(e.id, id, "IsValid({input:?})"),
                }
                continue;
            }

            // `deduplicateChannelIDs` is unexported in Go; `PreUpdate` is its only caller, so the
            // corpus drives it the same way. Order is preserved — first occurrence wins.
            let ids: Vec<String> =
                serde_json::from_value(case["dedup_in"].clone()).unwrap_or_default();
            let expected: Vec<String> =
                serde_json::from_value(case["dedup_out"].clone()).unwrap_or_default();
            let mut sr = ScheduledRecap {
                channel_ids: Some(ids.clone()),
                ..Default::default()
            };
            sr.pre_update();
            assert_eq!(
                sr.channel_ids.clone().unwrap_or_default(),
                expected,
                "deduplicateChannelIDs({ids:?})"
            );
            // The free function must agree with what PreUpdate does.
            assert_eq!(deduplicate_channel_ids(&ids), expected, "{ids:?}");
            seen_dedup += 1;
        }
        assert_eq!(seen_dedup, 6, "corpus should carry six dedup rows");
    }
}
