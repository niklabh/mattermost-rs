//! Port of `jobs/base_schedulers.go`, `jobs/schedulers.go` and `GenerateNextStartDateTime`
//! (jobs/jobs.go:340) — the half that *creates* jobs on a clock, as opposed to
//! [`crate::job_runtime`], which runs them.
//!
//! # Two schedulers are not safe the way two workers are
//!
//! [`crate::job_runtime`] explains why running this server's workers beside the Go server's is a
//! decided race: `ClaimJob` is one optimistic `UPDATE` and exactly one node wins. Schedulers have
//! no such guard. Go's protection is `isLeader` — `Schedulers.handleClusterLeaderChange` is fed by
//! the cluster interface, and only the elected leader has a non-nil `nextRunTimes`. That interface
//! is enterprise and nil on every build from this tree, which is why `initSchedulers` sets
//! `isLeader: true` outright (server.go:55): a single-node server is trivially the leader.
//!
//! Two single-node servers on one database are therefore **two leaders**, and each period would
//! queue its own job. `CheckForPendingJobsByType` does not prevent it — `PeriodicScheduler` and
//! `DailyScheduler` both ignore the `pendingJobs` argument entirely — so the duplicate is real.
//! That is the whole reason [`crate::job_runtime`] can be started here and this cannot; see the
//! `MM_API_ENABLE_JOB_WORKERS` note in `mm-api`'s `main.rs` and [D-802].
//!
//! # `NextScheduleTime` returning `None` is "never", not "now"
//!
//! `nextRunTimes[name] = nil` is the disabled state, and the loop `continue`s past it. A
//! `DailyScheduler` whose configured start time does not parse returns nil for exactly that
//! reason, so a malformed setting silently stops the job rather than running it every minute.

use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveTime, TimeZone, Timelike};
use mm_model::job::Job;

use crate::config::Config;

/// `jitterRange` (base_schedulers.go:80), in milliseconds.
pub const JITTER_RANGE_MS: u64 = 2000;

/// Port of `jobs.getRandomDelay` (base_schedulers.go:82): a uniform delay in `[0, limit)`
/// milliseconds, used to keep several servers' periodic schedulers from firing together.
///
/// Go draws from `crypto/rand` and falls back to **one millisecond** — not zero — if the draw
/// fails. The fallback is unreachable in practice and is reproduced because the alternative is
/// inventing a value.
pub fn random_delay(limit_ms: u64) -> Duration {
    if limit_ms == 0 {
        return Duration::from_millis(1);
    }
    Duration::from_millis(rand::Rng::random_range(&mut rand::rng(), 0..limit_ms))
}

/// Port of `jobs.GenerateNextStartDateTime` (jobs/jobs.go:340).
///
/// ```go
/// nextTime := time.Date(now.Year(), now.Month(), now.Day(),
///                       nextStartTime.Hour(), nextStartTime.Minute(), 0, 0, time.Local)
/// if !now.Before(nextTime) {
///     nextTime = nextTime.AddDate(0, 0, 1)
/// }
/// ```
///
/// # The start time is a wall clock, which is why this takes a [`NaiveTime`]
///
/// Go's parameter is a `time.Time`, and **only its `Hour()` and `Minute()` are read** — its date
/// and, more importantly, its *location* are discarded. The only caller in the public tree
/// produces it with `time.Parse("15:04", *cfg.ServiceSettings.RefreshPostStatsRunTime)`
/// (refresh_materialized_views/scheduler.go:15), which yields **1 Jan year 0 in UTC**; its
/// `Hour()` is then used as a *local* hour. So `"03:00"` means three in the morning in the
/// server's zone, not 03:00 UTC, and a port that converted the parsed value to local time before
/// reading the hour would shift every daily job by the server's offset. A `NaiveTime` argument
/// makes that mistake unrepresentable.
///
/// The result is in `Local` — the **server's** zone, [D-008] once more.
///
/// # Two more decisions, each with a fixture case behind it
///
/// - **The comparison is `!now.Before(nextTime)`, not `now.After`.** They differ on exact
///   equality: at precisely the scheduled instant the job is pushed to *tomorrow*, so a run that
///   completes inside the same minute cannot re-trigger.
/// - **`AddDate(0, 0, 1)` is calendar arithmetic, not `+24h`.** Across a daylight-saving change
///   the two differ by an hour, and the calendar answer is the one that keeps "03:00 every day"
///   at 03:00.
///
/// Returns `None` where Go's instant cannot exist here: a wall-clock time the local zone skips
/// over a spring-forward transition. Go's `time.Date` normalises such an input to the instant the
/// pre-transition offset names; `chrono` reports `LocalResult::None` and this answers `None`
/// rather than guessing, which stops that scheduler for the day instead of running it at an hour
/// nobody asked for. Recorded as [D-803]; unreachable in a zone without DST.
pub fn generate_next_start_date_time<Tz: TimeZone>(
    now: DateTime<Tz>,
    next_start_time: NaiveTime,
) -> Option<DateTime<Tz>> {
    let zone = now.timezone();
    let wall = NaiveTime::from_hms_opt(next_start_time.hour(), next_start_time.minute(), 0)?;

    // `.earliest()` and not `.single()`: a wall-clock time that the zone *repeats* — the hour
    // after a fall-back transition — is two instants, and Go's `time.Date` answers the **first**
    // of them. Measured, not assumed: `fall_back_target_is_ambiguous` in the fixture asks Go for
    // 01:30 on 1 November 2026 in America/New_York and gets `01:30-04:00`, the EDT one.
    let next = zone
        .with_ymd_and_hms(
            now.year(),
            now.month(),
            now.day(),
            wall.hour(),
            wall.minute(),
            0,
        )
        .earliest()?;

    if now < next {
        return Some(next);
    }
    // `AddDate(0, 0, 1)`: the same wall-clock time on the next calendar day, which across a
    // daylight-saving transition is *not* 24 hours later. See the fixture's
    // `spring_forward_rollover_keeps_the_wall_clock`, where Go's answer is 22 hours out.
    let tomorrow = next.date_naive().succ_opt()?.and_time(wall);
    tomorrow.and_local_timezone(zone).earliest()
}

/// Port of the `jobs.Scheduler` interface (schedulers.go:16).
///
/// `cfg`, `pending_jobs` and `last_successful_job` are on every method because the interface says
/// so; neither scheduler ported here reads any of them, and `base_schedulers.go` names all three
/// arguments `_` to say so out loud. They are kept because the enterprise schedulers — the LDAP
/// and message-export ones — do read them.
pub trait Scheduler: Send + Sync {
    /// The job type this scheduler queues. Go keys the map by the same string.
    fn job_type(&self) -> &'static str;

    /// Port of `Enabled(cfg *model.Config) bool`.
    fn enabled(&self, config: &Config) -> bool;

    /// Port of `NextScheduleTime`. `None` is Go's nil: never run.
    fn next_schedule_time(
        &self,
        config: &Config,
        now: DateTime<Local>,
        pending_jobs: bool,
        last_successful_job: Option<&Job>,
    ) -> Option<DateTime<Local>>;
}

/// Port of `jobs.PeriodicScheduler` (base_schedulers.go:15).
///
/// **The period is measured from now, not from the last run**, and the jitter is added *before*
/// the period: `time.Now().Add(jitter).Add(period)`. So a scheduler asked for its next time
/// repeatedly — which `handleConfigChange` does — walks the next run forward each time rather
/// than converging on a fixed schedule.
pub struct PeriodicScheduler {
    job_type: &'static str,
    period: Duration,
    enabled: fn(&Config) -> bool,
}

impl PeriodicScheduler {
    /// Port of `jobs.NewPeriodicScheduler` (base_schedulers.go:24).
    pub fn new(job_type: &'static str, period: Duration, enabled: fn(&Config) -> bool) -> Self {
        Self {
            job_type,
            period,
            enabled,
        }
    }

    pub fn period(&self) -> Duration {
        self.period
    }
}

impl Scheduler for PeriodicScheduler {
    fn job_type(&self) -> &'static str {
        self.job_type
    }

    fn enabled(&self, config: &Config) -> bool {
        (self.enabled)(config)
    }

    /// Note that `now` is **not** the argument Go uses: `NextScheduleTime` ignores its `now` and
    /// reads `time.Now()` itself (base_schedulers.go:37). Reproduced, because the difference is
    /// visible — the schedulers loop passes the timer's fire time, which lags the clock.
    fn next_schedule_time(
        &self,
        _config: &Config,
        _now: DateTime<Local>,
        _pending_jobs: bool,
        _last_successful_job: Option<&Job>,
    ) -> Option<DateTime<Local>> {
        let jitter = chrono::Duration::from_std(random_delay(JITTER_RANGE_MS)).ok()?;
        let period = chrono::Duration::from_std(self.period).ok()?;
        Local::now()
            .checked_add_signed(jitter)?
            .checked_add_signed(period)
    }
}

/// Port of `jobs.DailyScheduler` (base_schedulers.go:45): once a day at a configured wall-clock
/// time, through [`generate_next_start_date_time`].
///
/// `start_time` is a closure over the configuration rather than a stored value because Go's is:
/// `startTimeFunc func(cfg *model.Config) *time.Time`, re-read on every call, and **`nil` is a
/// legal answer** — see the module note on what a nil next time means.
pub struct DailyScheduler {
    job_type: &'static str,
    start_time: fn(&Config) -> Option<NaiveTime>,
    enabled: fn(&Config) -> bool,
}

impl DailyScheduler {
    /// Port of `jobs.NewDailyScheduler` (base_schedulers.go:54).
    pub fn new(
        job_type: &'static str,
        start_time: fn(&Config) -> Option<NaiveTime>,
        enabled: fn(&Config) -> bool,
    ) -> Self {
        Self {
            job_type,
            start_time,
            enabled,
        }
    }
}

impl Scheduler for DailyScheduler {
    fn job_type(&self) -> &'static str {
        self.job_type
    }

    fn enabled(&self, config: &Config) -> bool {
        (self.enabled)(config)
    }

    fn next_schedule_time(
        &self,
        config: &Config,
        now: DateTime<Local>,
        _pending_jobs: bool,
        _last_successful_job: Option<&Job>,
    ) -> Option<DateTime<Local>> {
        let scheduled = (self.start_time)(config)?;
        generate_next_start_date_time(now, scheduled)
    }
}

/// Port of `jobs/cleanup_desktop_tokens/scheduler.go`: hourly, always enabled.
pub fn cleanup_desktop_tokens_scheduler() -> PeriodicScheduler {
    /// `schedFreq` (scheduler.go:13).
    const SCHED_FREQ: Duration = Duration::from_secs(60 * 60);

    PeriodicScheduler::new(
        mm_model::job::JOB_TYPE_CLEANUP_DESKTOP_TOKENS,
        SCHED_FREQ,
        |_config| true,
    )
}

/// The schedulers this build knows about, matching [`crate::job_runtime::registered_workers`].
///
/// Nothing starts them; see the module note and [D-802].
pub fn registered_schedulers() -> Vec<Box<dyn Scheduler>> {
    vec![Box::new(cleanup_desktop_tokens_scheduler())]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Local> {
        Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(y, m, d)
                    .expect("valid date")
                    .and_hms_opt(h, min, 0)
                    .expect("valid time"),
            )
            .single()
            .expect("unambiguous in the test zone")
    }

    fn at(h: u32, min: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, min, 0).expect("valid time")
    }

    /// The scheduled time is still ahead today, so it is today.
    #[test]
    fn a_start_time_later_today_is_today() {
        let next = generate_next_start_date_time(local(2026, 3, 14, 9, 30), at(21, 0))
            .expect("representable");
        assert_eq!(next, local(2026, 3, 14, 21, 0));
    }

    /// Past for today, so tomorrow — and the *date* of the start time is ignored entirely.
    #[test]
    fn a_start_time_already_past_is_tomorrow_and_the_start_dates_day_is_ignored() {
        let next = generate_next_start_date_time(local(2026, 3, 14, 22, 15), at(21, 0))
            .expect("representable");
        assert_eq!(next, local(2026, 3, 15, 21, 0));
    }

    /// `!now.Before(next)` — at exactly the scheduled instant the answer is tomorrow, which
    /// `now.After(next)` would have got wrong by a whole day.
    #[test]
    fn exact_equality_schedules_tomorrow_not_now() {
        let next = generate_next_start_date_time(local(2026, 3, 14, 21, 0), at(21, 0))
            .expect("representable");
        assert_eq!(next, local(2026, 3, 15, 21, 0));
    }

    /// Seconds of the start time are dropped; only hour and minute survive, and the result's
    /// seconds are zero.
    #[test]
    fn only_the_hour_and_minute_of_the_start_time_are_read() {
        let start = NaiveTime::from_hms_opt(4, 7, 59).expect("valid time");
        let next =
            generate_next_start_date_time(local(2026, 3, 14, 3, 0), start).expect("representable");
        assert_eq!(next, local(2026, 3, 14, 4, 7));
        assert_eq!(next.second(), 0);
    }

    /// Crossing a month end is `AddDate`'s business, not a `+86400`.
    #[test]
    fn tomorrow_crosses_a_month_boundary() {
        let next = generate_next_start_date_time(local(2026, 1, 31, 23, 0), at(2, 0))
            .expect("representable");
        assert_eq!(next, local(2026, 2, 1, 2, 0));
    }

    /// `getRandomDelay` is bounded above by its limit and can be zero.
    #[test]
    fn the_jitter_is_inside_its_range() {
        for _ in 0..200 {
            let delay = random_delay(JITTER_RANGE_MS);
            assert!(delay < Duration::from_millis(JITTER_RANGE_MS));
        }
    }

    /// Go's fallback when the `crypto/rand` draw fails is one millisecond, not zero. A limit of
    /// zero is the only input that reaches it here.
    #[test]
    fn a_zero_limit_is_gos_one_millisecond_fallback() {
        assert_eq!(random_delay(0), Duration::from_millis(1));
    }

    /// The period is added to *now*, so the next run is always in the future, and jitter can only
    /// push it further out.
    #[test]
    fn the_periodic_next_time_is_now_plus_the_period_plus_jitter() {
        let scheduler = cleanup_desktop_tokens_scheduler();
        assert_eq!(scheduler.period(), Duration::from_secs(3600));
        let before = Local::now();
        let next = scheduler
            .next_schedule_time(&Config::default(), before, false, None)
            .expect("representable");
        let delta = next - before;
        assert!(delta >= chrono::Duration::seconds(3600), "{delta}");
        assert!(
            delta < chrono::Duration::seconds(3600) + chrono::Duration::milliseconds(2000 + 5000),
            "{delta}"
        );
    }

    #[test]
    fn the_registered_schedulers_match_the_registered_workers() {
        let schedulers = registered_schedulers();
        let workers = crate::job_runtime::registered_workers();
        assert_eq!(schedulers.len(), workers.len());
        for scheduler in &schedulers {
            assert!(
                workers.get(scheduler.job_type()).is_some(),
                "scheduler {} has no worker to run what it queues",
                scheduler.job_type()
            );
        }
    }
}

#[cfg(test)]
mod go_parity {
    //! `GenerateNextStartDateTime` against `fixtures/behaviour_job_scheduler.json`.
    //!
    //! Every case is replayed **in the zone the generator recorded**, not in the machine's: the
    //! function reads `time.Local`, so an assertion made in the local zone would pass or fail
    //! depending on where the test ran. That is the treatment [D-008] established for
    //! `FormatMillis`, and it is why [`generate_next_start_date_time`] is generic over
    //! `TimeZone` while every caller instantiates it at `Local`.

    use super::*;
    use chrono_tz::Tz;
    use serde_json::Value;
    use std::sync::OnceLock;

    fn oracle() -> &'static Value {
        static ORACLE: OnceLock<Value> = OnceLock::new();
        ORACLE.get_or_init(|| {
            let raw = include_str!("../../../fixtures/behaviour_job_scheduler.json");
            serde_json::from_str(raw).expect("behaviour_job_scheduler.json parses")
        })
    }

    /// Replay one recorded case in `zone` and return `(got, want)` as RFC3339 strings, with
    /// `None` on either side meaning "no next run time".
    fn replay(case: &Value, zone: Tz) -> (Option<String>, Option<String>) {
        let now = DateTime::parse_from_rfc3339(case["now"].as_str().expect("now is a string"))
            .expect("now is RFC3339")
            .with_timezone(&zone);
        let start = NaiveTime::from_hms_opt(
            case["start_hour"].as_u64().expect("start_hour") as u32,
            case["start_minute"].as_u64().expect("start_minute") as u32,
            0,
        )
        .expect("the recorded start time is valid");

        let got = generate_next_start_date_time(now, start).map(|t| t.to_rfc3339());
        let want = case["next"].as_str().map(|s| {
            DateTime::parse_from_rfc3339(s)
                .expect("next is RFC3339")
                .to_rfc3339()
        });
        (got, want)
    }

    fn cases(section: &str) -> &'static Vec<Value> {
        oracle()[section].as_array().expect("section is an array")
    }

    fn name(case: &Value) -> &str {
        case["name"].as_str().expect("name is a string")
    }

    /// The generator's own zone — `Asia/Kolkata`, pinned by `main.go` and recorded in the
    /// fixture so this does not have to be transcribed.
    fn generator_zone() -> Tz {
        oracle()["generator_zone"]["name"]
            .as_str()
            .expect("generator zone name")
            .parse()
            .expect("the recorded zone is an IANA name")
    }

    #[test]
    fn the_fixture_was_generated_in_the_zone_it_claims() {
        let zone = generator_zone();
        assert_eq!(zone.to_string(), "Asia/Kolkata");
        assert_eq!(oracle()["generator_zone"]["offset_seconds"], 19800);
    }

    /// The eleven no-daylight-saving cases, each an exact instant match.
    #[test]
    fn generate_next_start_date_time_matches_go() {
        let zone = generator_zone();
        let cases = cases("generate_next_start_date_time");
        assert_eq!(cases.len(), 11, "the oracle lost cases");
        for case in cases {
            let (got, want) = replay(case, zone);
            assert_eq!(got, want, "case {}", name(case));
        }
    }

    /// The same cases stated as a delta, which is what a `+24h`-instead-of-`AddDate` mutation
    /// moves without moving the wall clock in a zone that has no transitions.
    #[test]
    fn the_recorded_deltas_are_reproduced() {
        let zone = generator_zone();
        for case in cases("generate_next_start_date_time") {
            let now = DateTime::parse_from_rfc3339(case["now"].as_str().expect("now"))
                .expect("RFC3339")
                .with_timezone(&zone);
            let start = NaiveTime::from_hms_opt(
                case["start_hour"].as_u64().expect("start_hour") as u32,
                case["start_minute"].as_u64().expect("start_minute") as u32,
                0,
            )
            .expect("valid");
            let next = generate_next_start_date_time(now, start).expect("representable");
            assert_eq!(
                (next - now).num_seconds(),
                case["delta_seconds"].as_i64().expect("delta_seconds"),
                "case {}",
                name(case)
            );
            assert_eq!(
                next.date_naive() == now.date_naive(),
                case["next_is_same_day"]
                    .as_bool()
                    .expect("next_is_same_day"),
                "case {}",
                name(case)
            );
        }
    }

    /// The daylight-saving block, in `America/New_York`.
    ///
    /// Three of the four match Go exactly. The fourth — `spring_forward_target_is_skipped` — is
    /// the divergence [D-803] records, and it is asserted here rather than skipped, so that it
    /// cannot drift unnoticed in either direction:
    ///
    /// Go answers **01:30 EST** for a 02:30 start on the day the zone jumps 02:00 → 03:00. That
    /// is `time.Date`'s transition fix-up, which reaches for the offset on the *other* side of
    /// the boundary and lands an hour **before** the requested wall clock, not after. `chrono`
    /// declines to name an instant for a time that does not exist, so this returns `None`, and
    /// the scheduler treats that as "no next run" — one skipped day a year for a daily job,
    /// against Go running it an hour early. Neither is reachable in this build: the only
    /// `DailyScheduler` in the public tree is `refresh_materialized_views`, which is not ported.
    #[test]
    fn the_daylight_saving_cases_match_go_except_the_skipped_wall_clock() {
        let zone: Tz = oracle()["dst_zone"]
            .as_str()
            .expect("dst_zone")
            .parse()
            .expect("an IANA name");
        assert_eq!(zone.to_string(), "America/New_York");

        let cases = cases("generate_next_start_date_time_dst");
        assert_eq!(cases.len(), 4, "the oracle lost cases");
        for case in cases {
            let (got, want) = replay(case, zone);
            if name(case) == "spring_forward_target_is_skipped" {
                assert_eq!(
                    want.as_deref(),
                    Some("2026-03-08T01:30:00-05:00"),
                    "Go's answer for the skipped wall clock changed; [D-803] needs rereading"
                );
                assert_eq!(
                    got, None,
                    "[D-803]: a skipped wall clock has no instant here"
                );
                continue;
            }
            assert_eq!(got, want, "case {}", name(case));
        }
    }

    /// The fall-back hour happens twice and Go takes the **first**. `.single()` would answer
    /// `None` here and `.latest()` would answer the EST instant an hour later.
    #[test]
    fn the_repeated_wall_clock_is_the_earlier_of_the_two_instants() {
        let case = cases("generate_next_start_date_time_dst")
            .iter()
            .find(|c| name(c) == "fall_back_target_is_ambiguous")
            .expect("the ambiguous case is in the oracle");
        assert_eq!(case["next_zone"], "EDT");
        assert_eq!(case["next_offset_seconds"], -14400);

        let (got, want) = replay(case, chrono_tz::America::New_York);
        assert_eq!(got, want);
        assert_eq!(got.as_deref(), Some("2026-11-01T01:30:00-04:00"));
    }

    /// `time.Parse("15:04", …)` is the daily scheduler's whole input validation, and a failure is
    /// the `nil` start time that switches the scheduler off (base_schedulers.go:70). Recorded
    /// because the next person to port a `DailyScheduler` needs it, and because replaying it
    /// found a divergence that reading would not have.
    ///
    /// Ten of the eleven inputs agree between Go's `"15:04"` and chrono's `"%H:%M"`, including
    /// the two that surprise: `"3:00"` — a **one-digit hour** — is accepted by both, and
    /// `"0300"` is rejected by both.
    ///
    /// The eleventh does not. **`"03:0"` parses in chrono and fails in Go**: Go's `04` is a
    /// zero-padded two-digit minute and will not take one digit, while chrono's `%M` will. So a
    /// `RefreshPostStatsRunTime` of `"03:0"` switches the job off on the Go server and would
    /// schedule it for 03:00 on a port that reached for `%H:%M`. Nothing consumes this yet —
    /// `refresh_materialized_views` is the only `DailyScheduler` in the public tree and is not
    /// ported — so it is a trap recorded rather than a bug fixed; see [D-803].
    #[test]
    fn the_hhmm_parse_corpus_is_what_a_daily_scheduler_must_reproduce() {
        /// The one input on which chrono is more permissive than Go.
        const CHRONO_ACCEPTS_AND_GO_DOES_NOT: &str = "03:0";

        let mut accepted_by_go = 0;
        let mut divergences = Vec::new();
        for case in cases("parse_hhmm") {
            let input = case["input"].as_str().expect("input");
            let go_ok = case["ok"].as_bool().expect("ok");
            let parsed = NaiveTime::parse_from_str(input, "%H:%M");

            if parsed.is_ok() != go_ok {
                divergences.push(input);
                continue;
            }
            let Ok(parsed) = parsed else { continue };
            accepted_by_go += 1;
            assert_eq!(parsed.hour() as u64, case["hour"].as_u64().expect("hour"));
            assert_eq!(
                parsed.minute() as u64,
                case["minute"].as_u64().expect("minute")
            );
            // Go's parse lands in **UTC**, year 0 — and `GenerateNextStartDateTime` then reads
            // that UTC hour as a local one. See the function's doc comment.
            assert_eq!(case["zone"], "UTC");
            assert_eq!(case["year"], 0);
            assert_eq!(case["offset_seconds"], 0);
        }

        assert_eq!(accepted_by_go, 4, "four inputs are accepted by both");
        assert_eq!(
            divergences,
            vec![CHRONO_ACCEPTS_AND_GO_DOES_NOT],
            "the set of Go/chrono parse divergences changed; see [D-803]"
        );
    }

    /// The divergence stated on its own, so that the assertion above cannot be read as noise:
    /// Go's message names the layout element it could not fill.
    #[test]
    fn a_one_digit_minute_is_a_go_parse_failure() {
        let case = cases("parse_hhmm")
            .iter()
            .find(|c| c["input"] == "03:0")
            .expect("the one-digit-minute case is in the oracle");
        assert_eq!(case["ok"], false);
        assert_eq!(
            case["error"],
            "parsing time \"03:0\" as \"15:04\": cannot parse \"0\" as \"04\""
        );
        assert!(NaiveTime::parse_from_str("03:0", "%H:%M").is_ok());
    }
}
