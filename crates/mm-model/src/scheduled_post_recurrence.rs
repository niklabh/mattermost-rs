//! Port of `server/public/model/scheduled_post_recurrence.go` (40 lines) — **whole file**.
//!
//! Two constants, a one-line predicate, and `ComputeNextScheduledAt`, which is the file. The
//! constants live here rather than in [`crate::scheduled_post`], which borrowed them until this
//! module landed and now re-exports them — one definition, closing that half of [D-005].
//!
//! # The whole difficulty is that "a week later" is a wall-clock statement
//!
//! Go advances the schedule with `AddDate(0, 0, 7)`, which is `time.Date(y, m, d+7, …, loc)`: it
//! keeps the **local** time of day and re-resolves it in the zone. That is deliberate — a post
//! scheduled for 09:00 should stay at 09:00 after a DST change rather than drifting to 08:00 —
//! and it means the arithmetic can land on a local time that does not exist (spring forward) or
//! one that exists twice (autumn back).
//!
//! `time.Date`'s documentation declines to say what happens then: "the choice of time zone, and
//! therefore the time, is not guaranteed". So there is nothing to reason from, only something to
//! measure. [`crate::utils::go_time::date_in_zone`] carries the reproduction and the evidence;
//! the two results worth knowing here are that a **skipped** local hour resolves *backwards* in
//! America/New_York and *forwards* in Europe/London, and that a **repeated** local hour resolves
//! to the *earlier* instant in New York and the *later* one in London. Both splits are the sign
//! of the zone's offset, and both are the opposite of what `chrono`'s `LocalResult` arms suggest.
//!
//! # The loop re-reads the wall clock every step
//!
//! ```text
//! next := time.UnixMilli(s.ScheduledAt).In(loc).AddDate(0, 0, 7)
//! for !next.After(now) { next = next.AddDate(0, 0, 7) }
//! ```
//!
//! Each step adds seven days to the *previous answer's* wall clock, not to the original's. So
//! when a step is moved by an hour to escape a gap, every later step inherits the move — the
//! series is not `scheduled_at + 7n` days in any sense, and it cannot be computed by
//! multiplication. `four_steps_across_the_gap` in the oracle is the case that shows it.
//!
//! Note also that the first `AddDate` is unconditional: the answer is always at least a week
//! after `scheduled_at`, even when `scheduled_at` is itself in the future.

use std::str::FromStr;

use chrono::DateTime;
use chrono_tz::Tz;

use crate::scheduled_post::ScheduledPost;
use crate::utils::{go_quote, go_time};

/// Port of `model.ScheduledPostRepeatTypeNone` (scheduled_post_recurrence.go:12).
pub const SCHEDULED_POST_REPEAT_TYPE_NONE: &str = "";
/// Port of `model.ScheduledPostRepeatTypeWeekly` (scheduled_post_recurrence.go:13).
pub const SCHEDULED_POST_REPEAT_TYPE_WEEKLY: &str = "weekly";

/// The failure modes of `(*ScheduledPost).ComputeNextScheduledAt`
/// (scheduled_post_recurrence.go:22).
///
/// Go returns a bare `fmt.Errorf` for both, and both interpolate with `%q` — hence
/// [`go_quote`] rather than `{:?}`, which disagrees on control characters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ComputeNextScheduledAtError {
    /// Go's `failed to load repeat timezone %q: %w`, where the wrapped error is
    /// `time.LoadLocation`'s.
    ///
    /// Only the `unknown time zone <name>` form of that suffix is reproducible off Go's
    /// filesystem; a path-shaped name gets `time: invalid location name` or an OS error there and
    /// this form here. Same divergence, and the same reason, as `ScheduledPost::base_is_valid`'s
    /// — see [D-065].
    #[error("failed to load repeat timezone {}: unknown time zone {name}", go_quote(.name))]
    LoadTimezone { name: String },

    /// Go's `unsupported scheduled post repeat type %q`. Reached by every repeat type that is
    /// not `weekly`, the empty one included — a non-recurring post cannot be asked for its next
    /// occurrence.
    #[error("unsupported scheduled post repeat type {}", go_quote(.0))]
    UnsupportedRepeatType(String),

    /// Not Go's. See [`ScheduledPost::compute_next_scheduled_at`] and [D-068].
    #[error("scheduled post recurrence left the representable range of dates")]
    OutOfRange,
}

impl ScheduledPost {
    /// Port of `(*ScheduledPost).IsRecurring` (scheduled_post_recurrence.go:16).
    ///
    /// Exact equality against the one recurring type, so `"Weekly"` and `"weekly "` are both
    /// non-recurring — and both are also rejected by
    /// [`ScheduledPost::base_is_valid`](ScheduledPost::base_is_valid), which accepts only the
    /// same two values.
    pub fn is_recurring(&self) -> bool {
        self.repeat_type == SCHEDULED_POST_REPEAT_TYPE_WEEKLY
    }

    /// Port of `(*ScheduledPost).ComputeNextScheduledAt` (scheduled_post_recurrence.go:22).
    ///
    /// The next occurrence **strictly after** `now_millis`, in epoch milliseconds. Go's loop
    /// condition is `!next.After(now)`, so a candidate landing exactly on `now_millis` is
    /// rejected and another week is added.
    ///
    /// Two divergences from Go, both in states `base_is_valid` rejects before they can be
    /// reached:
    ///
    /// - `"Local"` is an error here and the **server's own zone** in Go, which is host state a
    ///   persisted schedule must not depend on. `""` is *not* a divergence: Go documents it as
    ///   UTC, so it is special-cased below rather than left to fail the lookup. See [D-065].
    /// - A `scheduled_at` far enough in the past that the loop would run past year 262143
    ///   returns [`ComputeNextScheduledAtError::OutOfRange`] where Go keeps looping — see
    ///   [D-068]. Reaching it takes a `now_millis` around 8e15, which `base_is_valid` cannot
    ///   admit and no clock produces.
    pub fn compute_next_scheduled_at(
        &self,
        now_millis: i64,
    ) -> Result<i64, ComputeNextScheduledAtError> {
        if self.repeat_type != SCHEDULED_POST_REPEAT_TYPE_WEEKLY {
            return Err(ComputeNextScheduledAtError::UnsupportedRepeatType(
                self.repeat_type.clone(),
            ));
        }

        let tz = load_location(&self.repeat_timezone).ok_or_else(|| {
            ComputeNextScheduledAtError::LoadTimezone {
                name: self.repeat_timezone.clone(),
            }
        })?;

        let scheduled = DateTime::from_timestamp_millis(self.scheduled_at)
            .ok_or(ComputeNextScheduledAtError::OutOfRange)?
            .with_timezone(&tz);

        // Unconditional: the first candidate is always a week out, however far in the future
        // `scheduled_at` already is.
        let mut next = go_time::add_date_days(&scheduled, 7).ok_or(OUT_OF_RANGE)?;

        // `!next.After(now)`. Both sides carry exactly millisecond precision — `UnixMilli` built
        // one and calendar arithmetic preserves the nanoseconds of the other — so comparing the
        // millisecond counts is the same comparison Go makes on instants.
        while next.timestamp_millis() <= now_millis {
            next = go_time::add_date_days(&next, 7).ok_or(OUT_OF_RANGE)?;
        }

        Ok(next.timestamp_millis())
    }
}

const OUT_OF_RANGE: ComputeNextScheduledAtError = ComputeNextScheduledAtError::OutOfRange;

/// The portable half of `time.LoadLocation`.
///
/// Go's is a filesystem lookup and has no single answer ([D-065]); `chrono_tz`'s embedded table
/// is what a Linux server with current tzdata effectively answers. The one name worth
/// special-casing is `""`, which Go **documents** as UTC — that is portable, so matching it is
/// free. `"Local"` is not, and is left to fail.
fn load_location(name: &str) -> Option<Tz> {
    if name.is_empty() {
        return Some(Tz::UTC);
    }
    Tz::from_str(name).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weekly(scheduled_at: i64, timezone: &str) -> ScheduledPost {
        ScheduledPost {
            scheduled_at,
            repeat_type: SCHEDULED_POST_REPEAT_TYPE_WEEKLY.to_string(),
            repeat_timezone: timezone.to_string(),
            ..ScheduledPost::default()
        }
    }

    #[test]
    fn is_recurring_is_exact_equality() {
        for (repeat_type, want) in [
            ("weekly", true),
            ("", false),
            ("Weekly", false),
            ("weekly ", false),
            ("daily", false),
        ] {
            let s = ScheduledPost {
                repeat_type: repeat_type.to_string(),
                ..ScheduledPost::default()
            };
            assert_eq!(s.is_recurring(), want, "{repeat_type:?}");
        }
    }

    #[test]
    fn every_repeat_type_but_weekly_is_unsupported() {
        for repeat_type in ["", "daily", "Weekly", "monthly"] {
            let s = ScheduledPost {
                repeat_type: repeat_type.to_string(),
                repeat_timezone: "UTC".to_string(),
                ..ScheduledPost::default()
            };
            assert_eq!(
                s.compute_next_scheduled_at(0),
                Err(ComputeNextScheduledAtError::UnsupportedRepeatType(
                    repeat_type.to_string()
                ))
            );
        }
    }

    /// The repeat type is checked before the timezone, so a garbage zone on a non-recurring post
    /// reports the type rather than the zone.
    #[test]
    fn the_repeat_type_is_checked_before_the_timezone() {
        let s = ScheduledPost {
            repeat_type: "daily".to_string(),
            repeat_timezone: "Nowhere/Nothing".to_string(),
            ..ScheduledPost::default()
        };
        assert!(matches!(
            s.compute_next_scheduled_at(0),
            Err(ComputeNextScheduledAtError::UnsupportedRepeatType(_))
        ));
    }

    #[test]
    fn the_error_messages_use_gos_quoting() {
        assert_eq!(
            ComputeNextScheduledAtError::UnsupportedRepeatType("a\"b\nc".to_string()).to_string(),
            r#"unsupported scheduled post repeat type "a\"b\nc""#
        );
        assert_eq!(
            ComputeNextScheduledAtError::LoadTimezone {
                name: "Nowhere/Nothing".to_string()
            }
            .to_string(),
            r#"failed to load repeat timezone "Nowhere/Nothing": unknown time zone Nowhere/Nothing"#
        );
    }

    /// Go documents `LoadLocation("")` as UTC, so the empty zone is not a lookup failure.
    #[test]
    fn an_empty_timezone_is_utc() {
        let s = weekly(1677915000000, "");
        let utc = weekly(1677915000000, "UTC");
        assert_eq!(
            s.compute_next_scheduled_at(1677915000000),
            utc.compute_next_scheduled_at(1677915000000)
        );
    }

    /// The divergence [D-065] names: Go resolves `Local` against the host and we refuse it.
    #[test]
    fn local_is_rejected_where_go_accepts_it() {
        let s = weekly(1677915000000, "Local");
        assert_eq!(
            s.compute_next_scheduled_at(0),
            Err(ComputeNextScheduledAtError::LoadTimezone {
                name: "Local".to_string()
            })
        );
    }

    /// A fixed-offset zone has nothing to resolve, so the answer is plain addition — which is
    /// the baseline every DST case below departs from.
    #[test]
    fn a_zone_without_dst_just_adds_seven_days() {
        let week = 7 * 24 * 60 * 60 * 1000;
        let s = weekly(1677915000000, "Asia/Kolkata");
        assert_eq!(
            s.compute_next_scheduled_at(1677915000000),
            Ok(1677915000000 + week)
        );
    }

    /// The first step is taken whether or not it is needed, so a schedule far in the future still
    /// moves a week.
    #[test]
    fn the_first_step_is_unconditional() {
        let week = 7 * 24 * 60 * 60 * 1000;
        let s = weekly(1677915000000, "UTC");
        assert_eq!(
            s.compute_next_scheduled_at(0),
            Ok(1677915000000 + week),
            "now is ten years before scheduled_at and a week is still added"
        );
    }

    /// `!next.After(now)` is strict: landing exactly on `now` costs another week.
    #[test]
    fn a_candidate_landing_exactly_on_now_is_rejected() {
        let week = 7 * 24 * 60 * 60 * 1000;
        let s = weekly(1677915000000, "UTC");

        assert_eq!(
            s.compute_next_scheduled_at(1677915000000 + week - 1),
            Ok(1677915000000 + week)
        );
        assert_eq!(
            s.compute_next_scheduled_at(1677915000000 + week),
            Ok(1677915000000 + 2 * week)
        );
    }
}

/// Parity tests driven by `fixtures/behaviour_scheduled_post_recurrence.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use chrono::{NaiveDate, Offset, TimeZone};
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_scheduled_post_recurrence.json"
        ))
        .unwrap()
    }

    fn zone(name: &str) -> Tz {
        Tz::from_str(name).unwrap_or_else(|_| panic!("chrono_tz does not know {name}"))
    }

    fn offset_seconds(tz: Tz, unix_seconds: i64) -> i64 {
        let instant = DateTime::from_timestamp(unix_seconds, 0).unwrap();
        tz.offset_from_utc_datetime(&instant.naive_utc())
            .fix()
            .local_minus_utc()
            .into()
    }

    #[test]
    fn the_constants_match_go() {
        let oracle = oracle();
        let c = &oracle["constants"];
        assert_eq!(
            c["ScheduledPostRepeatTypeNone"].as_str().unwrap(),
            SCHEDULED_POST_REPEAT_TYPE_NONE
        );
        assert_eq!(
            c["ScheduledPostRepeatTypeWeekly"].as_str().unwrap(),
            SCHEDULED_POST_REPEAT_TYPE_WEEKLY
        );
    }

    /// Every answer below is only as good as `chrono_tz`'s table agreeing with the tzdata the
    /// oracle ran against. This asserts it directly rather than assuming it: for each transition
    /// Go discovered, the offsets one second either side must match. A tzdata update that moved
    /// a transition fails here, where it is legible, instead of in a wall-clock assertion two
    /// tests down. Same class of guard as `scheduled_post`'s timezone-table test — see [D-065].
    #[test]
    fn chrono_tz_agrees_with_the_tzdata_the_oracle_ran_against() {
        let oracle = oracle();
        let cases = oracle["transitions"].as_array().unwrap();
        assert_eq!(cases.len(), 16, "the transition corpus changed size");

        for case in cases {
            let name = case["zone"].as_str().unwrap();
            let tz = zone(name);
            let at = case["at_unix"].as_i64().unwrap();
            let at_utc = case["at_utc"].as_str().unwrap();

            assert_eq!(
                offset_seconds(tz, at - 1),
                case["offset_before"].as_i64().unwrap(),
                "{name} before {at_utc}"
            );
            assert_eq!(
                offset_seconds(tz, at),
                case["offset_after"].as_i64().unwrap(),
                "{name} at {at_utc}"
            );
            // The bisection that found this instant assumed the offset was stable across the
            // fifteen minutes leading up to it. If it was not, the boundary is wrong and every
            // probe placed relative to it is testing something other than what it claims.
            assert_eq!(
                offset_seconds(tz, at - 15 * 60),
                case["offset_before"].as_i64().unwrap(),
                "{name}: a second transition inside the search window of {at_utc}"
            );
        }
    }

    #[test]
    fn is_recurring_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_recurring"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            assert!(!case["panicked"].as_bool().unwrap());
            let repeat_type = case["repeat_type"].as_str().unwrap();
            let s = ScheduledPost {
                repeat_type: repeat_type.to_string(),
                ..ScheduledPost::default()
            };
            assert_eq!(
                s.is_recurring(),
                case["is_recurring"].as_bool().unwrap(),
                "{repeat_type:?}"
            );
        }
    }

    /// The load-bearing test: `time.Date`'s normalisation over every wall clock in a two-hour
    /// window either side of all 16 discovered transitions, plus the hand-picked probes.
    ///
    /// It asserts the resulting **instant**, not the rendered local time — a port that resolved a
    /// gap to the wrong side would still render a plausible wall clock.
    #[test]
    fn time_date_matches_go() {
        let oracle = oracle();
        let cases = oracle["time_date"].as_array().unwrap();
        assert_eq!(cases.len(), 280, "the time.Date corpus changed size");

        let mut moved = 0;
        for case in cases {
            assert!(!case["panicked"].as_bool().unwrap());

            let name = case["zone"].as_str().unwrap();
            let tz = zone(name);
            let wall = NaiveDate::from_ymd_opt(
                case["year"].as_i64().unwrap() as i32,
                case["month"].as_u64().unwrap() as u32,
                case["day"].as_u64().unwrap() as u32,
            )
            .unwrap()
            .and_hms_nano_opt(
                case["hour"].as_u64().unwrap() as u32,
                case["min"].as_u64().unwrap() as u32,
                case["sec"].as_u64().unwrap() as u32,
                case["nsec"].as_u64().unwrap() as u32,
            )
            .unwrap();

            let got = go_time::date_in_zone(wall, &tz).unwrap();
            let label = format!("{name} {}", case["wall"].as_str().unwrap());

            assert_eq!(
                got.timestamp_millis(),
                case["unix_millis"].as_i64().unwrap(),
                "{label}: instant"
            );
            assert_eq!(
                i64::from(got.offset().fix().local_minus_utc()),
                case["offset_seconds"].as_i64().unwrap(),
                "{label}: offset"
            );

            if !case["resolved_wall_matches_input"].as_bool().unwrap() {
                moved += 1;
            }
        }

        // The corpus is only worth anything if it actually contains skipped local times. If a
        // tzdata change stopped it doing so, the test above would pass while proving nothing.
        assert_eq!(moved, 34, "the number of non-existent local times changed");
    }

    /// The two intuitions the corpus refutes, asserted by name so they cannot quietly stop being
    /// tested. Both are the sign of the zone's offset, not a property of gaps or folds.
    #[test]
    fn a_skipped_local_hour_resolves_backwards_in_new_york_and_forwards_in_london() {
        let ny = zone("America/New_York");
        let london = zone("Europe/London");

        // 2023-03-12 02:30 does not exist in New York; Go answers 01:30 EST, before the gap.
        let wall = NaiveDate::from_ymd_opt(2023, 3, 12)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        let got = go_time::date_in_zone(wall, &ny).unwrap();
        assert_eq!(got.to_string(), "2023-03-12 01:30:00 EST");

        // 2023-03-26 01:30 does not exist in London; Go answers 02:30 BST, after it.
        let wall = NaiveDate::from_ymd_opt(2023, 3, 26)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        let got = go_time::date_in_zone(wall, &london).unwrap();
        assert_eq!(got.to_string(), "2023-03-26 02:30:00 BST");
    }

    #[test]
    fn a_repeated_local_hour_takes_the_earlier_instant_in_new_york_and_the_later_in_london() {
        let ny = zone("America/New_York");
        let london = zone("Europe/London");

        // 01:30 happens twice on 2023-11-05 in New York: 05:30Z (EDT) then 06:30Z (EST).
        let wall = NaiveDate::from_ymd_opt(2023, 11, 5)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        let got = go_time::date_in_zone(wall, &ny).unwrap();
        assert_eq!(got.timestamp_millis(), 1699162200000, "the earlier one");

        // 01:30 happens twice on 2023-10-29 in London: 00:30Z (BST) then 01:30Z (GMT).
        let wall = NaiveDate::from_ymd_opt(2023, 10, 29)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        let got = go_time::date_in_zone(wall, &london).unwrap();
        assert_eq!(got.timestamp_millis(), 1698543000000, "the later one");
    }

    /// `chrono`'s own resolver disagrees with Go on both, which is why [`go_time::date_in_zone`]
    /// exists rather than a `LocalResult` match. Asserted so that a future simplification to
    /// `.earliest()` fails a test that says why.
    #[test]
    fn chronos_local_result_is_not_a_substitute() {
        use chrono::offset::LocalResult;

        let london = zone("Europe/London");

        // The gap: chrono has no answer at all.
        let wall = NaiveDate::from_ymd_opt(2023, 3, 26)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        assert!(matches!(
            london.from_local_datetime(&wall),
            LocalResult::None
        ));

        // The fold: chrono offers both, and the obvious pick is the wrong one.
        let wall = NaiveDate::from_ymd_opt(2023, 10, 29)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        let LocalResult::Ambiguous(earliest, latest) = london.from_local_datetime(&wall) else {
            panic!("expected an ambiguous local time");
        };
        assert_ne!(earliest.timestamp_millis(), 1698543000000);
        assert_eq!(latest.timestamp_millis(), 1698543000000);
    }

    #[test]
    fn compute_next_scheduled_at_matches_go() {
        let oracle = oracle();
        let cases = oracle["compute_next"].as_array().unwrap();
        assert_eq!(cases.len(), 295, "the compute_next corpus changed size");

        // The names whose Go answer is a host artifact rather than a property of Go. Both are
        // rejected by `base_is_valid` long before `ComputeNextScheduledAt` runs. See [D-065].
        const HOST_DEPENDENT: [&str; 2] = ["timezone Local", "timezone lowercase"];

        let mut errors = 0;
        for case in cases {
            assert!(!case["panicked"].as_bool().unwrap());

            let name = case["name"].as_str().unwrap();
            let s = ScheduledPost {
                scheduled_at: case["scheduled_at"].as_i64().unwrap(),
                repeat_type: case["repeat_type"].as_str().unwrap().to_string(),
                repeat_timezone: case["timezone"].as_str().unwrap().to_string(),
                ..ScheduledPost::default()
            };
            let got = s.compute_next_scheduled_at(case["now"].as_i64().unwrap());

            if HOST_DEPENDENT.contains(&name) {
                assert!(
                    case["ok"].as_bool().unwrap(),
                    "{name}: Go used to accept it"
                );
                assert!(got.is_err(), "{name}: expected the documented divergence");
                continue;
            }

            if !case["ok"].as_bool().unwrap() {
                errors += 1;
                let err = got.expect_err(&format!("{name}: Go failed and we did not"));
                assert!(
                    matches!(
                        err,
                        ComputeNextScheduledAtError::UnsupportedRepeatType(_)
                            | ComputeNextScheduledAtError::LoadTimezone { .. }
                    ),
                    "{name}: {err}"
                );
                // The error text is reproduced exactly for the repeat-type arm; the timezone
                // arm's suffix is Go's filesystem talking, so only the prefix is compared.
                let want = case["err"].as_str().unwrap();
                if matches!(err, ComputeNextScheduledAtError::UnsupportedRepeatType(_)) {
                    assert_eq!(err.to_string(), want, "{name}");
                } else {
                    let prefix = want.split(':').next().unwrap();
                    assert!(err.to_string().starts_with(prefix), "{name}: {err}");
                }
                continue;
            }

            assert_eq!(
                got.unwrap_or_else(|e| panic!("{name}: {e}")),
                case["next"].as_i64().unwrap(),
                "{name}: next (Go's wall clock was {:?})",
                case["next_wall"]
            );
        }

        assert!(errors >= 6, "the error corpus shrank to {errors} cases");
    }

    /// The series preserves the **wall clock**, not the elapsed time, so it is not
    /// `scheduled_at + 7n` days and cannot be computed by multiplication.
    ///
    /// Four steps from an EST wall clock land on the same local time of day in EDT, which is
    /// four weeks *minus an hour* of real time. Read off the oracle rather than reasoned about.
    #[test]
    fn the_series_is_not_a_multiple_of_a_week() {
        let oracle = oracle();
        let cases = oracle["compute_next"].as_array().unwrap();

        let find = |name: &str| {
            cases
                .iter()
                .find(|c| c["name"].as_str() == Some(name))
                .unwrap_or_else(|| panic!("{name} is missing from the corpus"))
        };

        let one = find("next lands one ms after now");
        let four = find("four steps across the gap");

        let week = 7 * 24 * 60 * 60 * 1000i64;
        let scheduled = one["scheduled_at"].as_i64().unwrap();

        assert_eq!(one["next"].as_i64().unwrap(), scheduled + week);
        assert_eq!(
            four["next"].as_i64().unwrap(),
            scheduled + 4 * week - 3_600_000,
            "four weeks of wall clock is an hour short of four weeks of elapsed time"
        );

        // What is conserved is the local time of day, across all three timestamps.
        for case in [one, four] {
            assert!(
                case["next_wall"]
                    .as_str()
                    .unwrap()
                    .ends_with("T02:30:00.000"),
                "{}",
                case["next_wall"]
            );
        }
        assert!(
            one["scheduled_wall"]
                .as_str()
                .unwrap()
                .ends_with("T02:30:00.000")
        );
    }
}
