package main

// Behavioural oracle for the job **schedulers** — `jobs.GenerateNextStartDateTime` and the
// `time.Parse("15:04", …)` that feeds it — written to fixtures/behaviour_job_scheduler.json.
//
// # Why this needs an oracle at all
//
//	nextTime := time.Date(now.Year(), now.Month(), now.Day(),
//	                      nextStartTime.Hour(), nextStartTime.Minute(), 0, 0, time.Local)
//	if !now.Before(nextTime) {
//	    nextTime = nextTime.AddDate(0, 0, 1)
//	}
//
// Four lines, and every one of them hides a decision that reads the other way round:
//
//   - `nextStartTime` contributes **only** its hour and minute. Its date is dropped and its
//     *location* is dropped — the only caller in the public tree builds it with
//     `time.Parse("15:04", …)`, which returns 1 Jan year 0 **in UTC**, and that UTC hour is then
//     used as a `time.Local` hour. "03:00" means 3am on the server, not 03:00Z.
//   - `!now.Before(x)` is not `now.After(x)`: they disagree at exact equality, and equality is
//     reachable — a minute-granularity schedule fires on a whole minute.
//   - `AddDate(0, 0, 1)` is calendar arithmetic. On a day that is 23 or 25 hours long it is not
//     `+24h`, and the difference is an hour of drift per transition.
//   - `time.Date` with a wall-clock time the zone **skips** (spring forward) or **repeats** (fall
//     back) has to pick something. What it picks is the whole reason the DST block below exists:
//     it is not stated in the documentation, which says only that the choice "is not guaranteed".
//
// # The DST block reassigns `time.Local`
//
// `GenerateNextStartDateTime` hard-codes `time.Local`, so the only way to ask Go what it does at a
// daylight-saving transition is to move `time.Local` to a zone that has one. `time.Local` is a
// package-level `var`, so that is legal; it is restored before this function returns and the
// restoration is asserted, because main.go pins the zone for every other fixture in the run.
//
// Determinism: fixed inputs, fixed zones. Nothing reads the clock.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/mattermost/mattermost/server/v8/channels/jobs"
)

// The zone the DST cases run in. Chosen for a well-known transition rule and a whole-hour offset
// on both sides, so a reader can check the arithmetic by eye.
const dstZone = "America/New_York"

func writeJobSchedulerBehaviourFixture(outDir string) error {
	local, err := jobSchedulerLocalCases()
	if err != nil {
		return err
	}
	dst, err := jobSchedulerDSTCases()
	if err != nil {
		return err
	}

	out := map[string]any{
		"generator_zone":                    jobSchedulerZone(time.Local),
		"dst_zone":                          dstZone,
		"generate_next_start_date_time":     local,
		"generate_next_start_date_time_dst": dst,
		"parse_hhmm":                        jobSchedulerParseCases(),
	}

	blob, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		return fmt.Errorf("marshal job scheduler behaviour: %w", err)
	}
	blob = append(blob, '\n')
	return os.WriteFile(filepath.Join(outDir, "behaviour_job_scheduler.json"), blob, 0o644)
}

func jobSchedulerZone(loc *time.Location) map[string]any {
	// The offset is sampled at a fixed instant so that a zone with DST reports a stable one.
	_, offset := time.Date(2026, 1, 15, 12, 0, 0, 0, loc).Zone()
	return map[string]any{
		"name":           loc.String(),
		"offset_seconds": offset,
	}
}

// One probe: a wall-clock `now` in the current `time.Local`, and a start time given as hour and
// minute — which is all `GenerateNextStartDateTime` reads of its second argument.
type jobSchedulerProbe struct {
	name   string
	year   int
	month  time.Month
	day    int
	hour   int
	minute int
	second int
	// The start time, as the hour and minute a settings string would parse to.
	startHour   int
	startMinute int
}

func jobSchedulerProbes() []jobSchedulerProbe {
	return []jobSchedulerProbe{
		{name: "later_today", year: 2026, month: time.March, day: 14, hour: 9, minute: 30, startHour: 21},
		{name: "earlier_today_is_tomorrow", year: 2026, month: time.March, day: 14, hour: 22, minute: 15, startHour: 21},
		// `!now.Before(next)`: equality goes to tomorrow. `now.After(next)` would answer today.
		{name: "exact_equality_is_tomorrow", year: 2026, month: time.March, day: 14, hour: 21, startHour: 21},
		{name: "one_second_before_is_today", year: 2026, month: time.March, day: 14, hour: 20, minute: 59, second: 59, startHour: 21},
		{name: "one_second_after_is_tomorrow", year: 2026, month: time.March, day: 14, hour: 21, minute: 0, second: 1, startHour: 21},
		// Midnight against midnight: equality again, and the rollover crosses a date.
		{name: "midnight_against_midnight", year: 2026, month: time.March, day: 14, startHour: 0},
		{name: "month_end_rolls_into_the_next_month", year: 2026, month: time.January, day: 31, hour: 23, startHour: 2},
		{name: "year_end_rolls_into_the_next_year", year: 2026, month: time.December, day: 31, hour: 23, minute: 30, startHour: 1},
		{name: "february_28_rolls_into_a_leap_day", year: 2028, month: time.February, day: 28, hour: 23, startHour: 3},
		{name: "a_non_zero_start_minute_survives", year: 2026, month: time.March, day: 14, hour: 3, startHour: 4, startMinute: 7},
		// The seconds of `now` are compared but never copied: the answer is always on a whole
		// minute with zero seconds and zero nanoseconds.
		{name: "the_answer_has_no_seconds", year: 2026, month: time.March, day: 14, hour: 1, minute: 2, second: 3, startHour: 5, startMinute: 6},
	}
}

func jobSchedulerRun(p jobSchedulerProbe) map[string]any {
	now := time.Date(p.year, p.month, p.day, p.hour, p.minute, p.second, 0, time.Local)
	// Built exactly as `refresh_materialized_views`' scheduler builds it: parsed from "15:04",
	// which lands in UTC. Passing a UTC value on purpose — the point is that only Hour/Minute
	// are read, so the zone it carries is irrelevant to the answer.
	start, err := time.Parse("15:04", fmt.Sprintf("%02d:%02d", p.startHour, p.startMinute))
	if err != nil {
		return map[string]any{"name": p.name, "parse_error": err.Error()}
	}
	next := jobs.GenerateNextStartDateTime(now, start)

	zoneName, zoneOffset := now.Zone()
	out := map[string]any{
		"name":               p.name,
		"now":                now.Format(time.RFC3339),
		"now_unix_milli":     now.UnixMilli(),
		"now_zone":           zoneName,
		"now_offset_seconds": zoneOffset,
		"start_source":       start.Format(time.RFC3339),
		"start_hour":         start.Hour(),
		"start_minute":       start.Minute(),
	}
	if next == nil {
		out["next"] = nil
		return out
	}
	nextZone, nextOffset := next.Zone()
	out["next"] = next.Format(time.RFC3339)
	out["next_unix_milli"] = next.UnixMilli()
	out["next_zone"] = nextZone
	out["next_offset_seconds"] = nextOffset
	out["next_is_same_day"] = next.Day() == now.Day() && next.Month() == now.Month() && next.Year() == now.Year()
	// `AddDate(0,0,1)` against `+24h`: on a transition day these differ, and the field says which
	// one ran.
	out["delta_seconds"] = int64(next.Sub(now) / time.Second)
	return out
}

func jobSchedulerLocalCases() ([]map[string]any, error) {
	out := make([]map[string]any, 0, len(jobSchedulerProbes()))
	for _, p := range jobSchedulerProbes() {
		out = append(out, jobSchedulerRun(p))
	}
	return out, nil
}

// The daylight-saving probes, run with `time.Local` moved to a zone that has transitions.
//
// 2026 in America/New_York: forward on 8 March (02:00 → 03:00, so 02:00–02:59 does not exist) and
// back on 1 November (02:00 → 01:00, so 01:00–01:59 happens twice).
func jobSchedulerDSTCases() ([]map[string]any, error) {
	loc, err := time.LoadLocation(dstZone)
	if err != nil {
		return nil, fmt.Errorf("load %s (is tzdata installed?): %w", dstZone, err)
	}

	saved := time.Local
	time.Local = loc
	defer func() { time.Local = saved }()

	probes := []jobSchedulerProbe{
		// The target wall-clock time does not exist on this day. `time.Date` has to invent one.
		{name: "spring_forward_target_is_skipped", year: 2026, month: time.March, day: 8, hour: 0, minute: 30, startHour: 2, startMinute: 30},
		// Rolling over *into* the 23-hour day. `AddDate` keeps the wall clock; `+24h` would not.
		{name: "spring_forward_rollover_keeps_the_wall_clock", year: 2026, month: time.March, day: 7, hour: 23, startHour: 22},
		// The target happens twice. `time.Date` has to pick one of the two offsets.
		{name: "fall_back_target_is_ambiguous", year: 2026, month: time.November, day: 1, hour: 0, minute: 30, startHour: 1, startMinute: 30},
		// Rolling over into the 25-hour day.
		{name: "fall_back_rollover_keeps_the_wall_clock", year: 2026, month: time.October, day: 31, hour: 23, startHour: 22},
	}

	out := make([]map[string]any, 0, len(probes))
	for _, p := range probes {
		out = append(out, jobSchedulerRun(p))
	}

	// Restored by the defer above; assert it here so a future edit that returns early cannot
	// leave the rest of the run in the wrong zone.
	if time.Local != loc {
		return nil, fmt.Errorf("time.Local changed underneath the DST block")
	}
	return out, nil
}

// `time.Parse("15:04", s)` — the daily scheduler's whole input validation. A parse failure is the
// `nil` start time that switches the scheduler off (base_schedulers.go:70).
func jobSchedulerParseCases() []map[string]any {
	inputs := []string{"03:00", "00:00", "23:59", "3:00", "03:0", "24:00", "23:60", "", "0300", "03:00:00", "abc"}
	out := make([]map[string]any, 0, len(inputs))
	for _, in := range inputs {
		parsed, err := time.Parse("15:04", in)
		row := map[string]any{"input": in, "ok": err == nil}
		if err != nil {
			row["error"] = err.Error()
			out = append(out, row)
			continue
		}
		zone, offset := parsed.Zone()
		row["rfc3339"] = parsed.Format(time.RFC3339)
		row["hour"] = parsed.Hour()
		row["minute"] = parsed.Minute()
		row["year"] = parsed.Year()
		row["zone"] = zone
		row["offset_seconds"] = offset
		out = append(out, row)
	}
	return out
}
