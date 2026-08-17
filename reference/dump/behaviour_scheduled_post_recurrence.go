package main

// Behavioural oracle for model/scheduled_post_recurrence.go, written to
// fixtures/behaviour_scheduled_post_recurrence.json.
//
// The file is 40 lines and 30 of them are `ComputeNextScheduledAt`, whose whole difficulty is
// that it does calendar arithmetic in a named zone:
//
//	next := time.UnixMilli(s.ScheduledAt).In(loc).AddDate(0, 0, 7)
//
// `AddDate` is `time.Date(y, m, d+7, h, mi, s, ns, loc)` — it keeps the **wall clock** and
// re-resolves it, so a schedule that crosses a DST boundary lands on a local time that may not
// exist (spring forward) or may exist twice (autumn back). `time.Date`'s doc says only that in
// such cases "the choice of time zone, and therefore the time, is not guaranteed", so there is
// no specification to port — there is only what the implementation does, which is:
//
//	unix := <the wall clock read as if it were UTC>
//	_, offset, start, end, _ := loc.lookup(unix)
//	if offset != 0 {
//	    switch utc := unix - int64(offset); {
//	    case utc < start: _, offset, _, _, _ = loc.lookup(start - 1)
//	    case utc >= end:  _, offset, _, _, _ = loc.lookup(end)
//	    }
//	    unix -= int64(offset)
//	}
//
// Reading that produces three wrong intuitions, and this corpus exists to correct them. All
// three come from the same place: the lookup is done on the wall clock **read as a UTC
// instant**, so which side of the transition Go lands on is decided by the sign of the zone's
// own offset.
//
//   - **A skipped local hour does not resolve forwards.** In America/New_York, 02:30 on
//     2023-03-12 does not exist, and Go answers 01:30 EST — *before* the gap, not after it.
//   - **...except where it does.** In Europe/London, 01:30 on 2023-03-26 does not exist either,
//     and Go answers 02:30 BST. Antarctica/Troll (+0/+2) is the sharpest case of this, because
//     its winter offset is 0 and Go's `if offset != 0` skips the correction outright.
//   - **An ambiguous local hour does not resolve to the earlier instant.** It resolves to the
//     earlier one in America/New_York and America/St_Johns, and to the **later** one in every
//     positive-offset zone here — London, Troll, Casablanca, Sydney, Lord Howe and Chatham.
//     Measured over all 34 ambiguous probes; the split is exactly the sign of the offset in
//     force before the transition, with no exceptions.
//
// The corpus is generated rather than hand-listed: the DST transitions are discovered by
// scanning the host's tzdata, then every probe is placed relative to a discovered transition.
// That keeps it honest about which boundary it is testing, and the discovered transitions are
// written out as their own section so the Rust side can assert that `chrono-tz`'s embedded table
// agrees with the host's before trusting any of the answers — [D-065] is about exactly that gap.
//
// Determinism: every input is a fixed constant or is derived from tzdata. No `time.Now`, no
// `NewId` — see [D-032].

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeScheduledPostRecurrenceBehaviourFixture(outDir string) error {
	zones, err := recurrenceZones()
	if err != nil {
		return err
	}

	out := map[string]any{
		"constants":    recurrenceConstants(),
		"is_recurring": recurrenceIsRecurringAll(),
		"transitions":  recurrenceTransitionsAll(zones),
		"time_date":    recurrenceTimeDateAll(zones),
		"compute_next": recurrenceComputeNextAll(zones),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_scheduled_post_recurrence.json"), append(blob, '\n'), 0o644)
}

// --- constants -------------------------------------------------------------------------------

// Both constants live in this file in Go. `scheduled_post.go`'s IsValid switch is their only
// other consumer, and the Rust port borrowed them into scheduled_post.rs until this file landed
// — see [D-005]. Recorded here so their home file's oracle owns them.
func recurrenceConstants() map[string]any {
	return map[string]any{
		"ScheduledPostRepeatTypeNone":   model.ScheduledPostRepeatTypeNone,
		"ScheduledPostRepeatTypeWeekly": model.ScheduledPostRepeatTypeWeekly,
	}
}

// --- IsRecurring -----------------------------------------------------------------------------

func recurrenceIsRecurringAll() []map[string]any {
	types := []string{
		"", "weekly", "Weekly", "WEEKLY", "weekly ", " weekly", "daily", "monthly", "none", "0",
	}

	var res []map[string]any
	for _, repeatType := range types {
		s := &model.ScheduledPost{RepeatType: repeatType}
		row := map[string]any{"repeat_type": repeatType}
		probe(row, func() {
			row["is_recurring"] = s.IsRecurring()
		})
		res = append(res, row)
	}
	return res
}

// --- the zone corpus -------------------------------------------------------------------------

// recurrenceZone pairs a loaded location with the transitions discovered in it.
type recurrenceZone struct {
	name        string
	loc         *time.Location
	transitions []recurrenceTransition
}

type recurrenceTransition struct {
	at   int64 // unix seconds of the instant the offset changes
	prev int   // offset in seconds before it
	next int   // offset in seconds after it
}

// The zones are chosen for the shape of their transitions, not for their popularity:
//
//	America/New_York   negative offset, one-hour DST — the canonical case
//	America/St_Johns   negative offset at :30, so the wall clock is not on an hour boundary
//	Europe/London      positive offset, and the winter offset is 0
//	Antarctica/Troll   +0/+2, a TWO-hour gap, and Go's `offset != 0` guard skips the correction
//	Africa/Casablanca  +0/+1 with two transition PAIRS in 2023 (Ramadan), so four in one year
//	Asia/Kolkata       +5:30 and no DST at all — the control
//	Pacific/Kiritimati +14, the largest offset there is
//	Australia/Sydney   southern hemisphere, so the transitions run the other way round
//	Australia/Lord_Howe a THIRTY-minute DST shift, the only one in tzdata
//	Pacific/Chatham    +12:45/+13:45, offsets that are not a whole number of half-hours
func recurrenceZones() ([]recurrenceZone, error) {
	names := []string{
		"America/New_York",
		"America/St_Johns",
		"Europe/London",
		"Antarctica/Troll",
		"Africa/Casablanca",
		"Asia/Kolkata",
		"Pacific/Kiritimati",
		"Australia/Sydney",
		"Australia/Lord_Howe",
		"Pacific/Chatham",
	}

	var res []recurrenceZone
	for _, name := range names {
		loc, err := time.LoadLocation(name)
		if err != nil {
			return nil, fmt.Errorf("loading %s: %w", name, err)
		}
		res = append(res, recurrenceZone{
			name:        name,
			loc:         loc,
			transitions: findTransitions(loc, 2023),
		})
	}
	return res, nil
}

// findTransitions returns every offset change in the given year, discovered by scanning rather
// than transcribed. Go exposes no transition table, so the offset function is sampled at
// fifteen-minute steps and each change is then bisected down to the exact second.
//
// The bisection assumes at most one change per fifteen-minute window, which holds for every zone
// in tzdata; a zone that broke it would show up as a transition whose recorded `prev` does not
// match the previous row's `next`, which the Rust side asserts.
func findTransitions(loc *time.Location, year int) []recurrenceTransition {
	const step = int64(15 * 60)

	start := time.Date(year, 1, 1, 0, 0, 0, 0, time.UTC).Unix()
	end := time.Date(year+1, 1, 1, 0, 0, 0, 0, time.UTC).Unix()

	var res []recurrenceTransition
	prev := offsetAt(loc, start)
	for t := start + step; t <= end; t += step {
		off := offsetAt(loc, t)
		if off == prev {
			continue
		}

		lo, hi := t-step, t
		for hi-lo > 1 {
			mid := lo + (hi-lo)/2
			if offsetAt(loc, mid) == prev {
				lo = mid
			} else {
				hi = mid
			}
		}

		res = append(res, recurrenceTransition{at: hi, prev: prev, next: off})
		prev = off
	}
	return res
}

func offsetAt(loc *time.Location, unix int64) int {
	_, offset := time.Unix(unix, 0).In(loc).Zone()
	return offset
}

func recurrenceTransitionsAll(zones []recurrenceZone) []map[string]any {
	var res []map[string]any
	for _, zone := range zones {
		for _, tr := range zone.transitions {
			res = append(res, map[string]any{
				"zone":            zone.name,
				"at_unix":         tr.at,
				"at_utc":          time.Unix(tr.at, 0).UTC().Format(time.RFC3339),
				"offset_before":   tr.prev,
				"offset_after":    tr.next,
				"gap_seconds":     tr.next - tr.prev,
				"wall_before":     wallString(time.Unix(tr.at-1, 0).In(zone.loc)),
				"wall_after":      wallString(time.Unix(tr.at, 0).In(zone.loc)),
				"abbrev_after":    zoneAbbrev(time.Unix(tr.at, 0).In(zone.loc)),
				"transition_kind": transitionKind(tr),
			})
		}
	}
	return res
}

func transitionKind(tr recurrenceTransition) string {
	switch {
	case tr.next > tr.prev:
		return "gap" // wall clock jumps forward; the skipped local times do not exist
	case tr.next < tr.prev:
		return "fold" // wall clock jumps back; the repeated local times happen twice
	default:
		return "none"
	}
}

func wallString(t time.Time) string {
	return t.Format("2006-01-02T15:04:05.000")
}

func zoneAbbrev(t time.Time) string {
	name, _ := t.Zone()
	return name
}

// --- time.Date -------------------------------------------------------------------------------

// wallProbes returns the local wall-clock values to drive, as (year, month, …) components. They
// are placed relative to a discovered transition: `wallBase` is the wall clock at the instant of
// the transition read in the OLD offset, so k=0 is the first non-existent (or first repeated)
// local time and the window reaches two hours either side of it.
//
// The values are produced by reading a unix instant in UTC purely to decompose it into calendar
// fields — the instant itself is meaningless, only the fields are used.
func wallProbes(tr recurrenceTransition) []time.Time {
	wallBase := tr.at + int64(tr.prev)

	var res []time.Time
	for k := -8; k <= 8; k++ {
		res = append(res, time.Unix(wallBase+int64(k)*15*60, 0).UTC())
	}
	return res
}

func recurrenceTimeDateAll(zones []recurrenceZone) []map[string]any {
	var res []map[string]any

	for _, zone := range zones {
		for i, tr := range zone.transitions {
			for _, w := range wallProbes(tr) {
				res = append(res, timeDateRow(zone, fmt.Sprintf("%s#%d", zone.name, i), w, 0))
			}
		}
	}

	// Hand-picked probes that no transition window reaches: sub-second nanoseconds, a date far
	// from any boundary, and two pre-1970 instants. The 1850 probes land in the zones' LMT
	// records, whose offsets are not a whole number of minutes (New York is -4:56:02), which is
	// where an implementation that rounds anywhere will part company with Go.
	extras := []struct {
		zone string
		wall time.Time
		nsec int
	}{
		{"America/New_York", time.Date(2023, 6, 15, 12, 0, 0, 0, time.UTC), 123_000_000},
		{"America/New_York", time.Date(1850, 3, 12, 2, 30, 0, 0, time.UTC), 0},
		{"America/New_York", time.Date(1900, 1, 1, 0, 0, 0, 0, time.UTC), 0},
		{"Europe/London", time.Date(1850, 3, 26, 1, 30, 0, 0, time.UTC), 0},
		{"Asia/Kolkata", time.Date(2023, 2, 28, 23, 59, 59, 0, time.UTC), 999_000_000},
		{"Pacific/Kiritimati", time.Date(2023, 12, 31, 23, 30, 0, 0, time.UTC), 0},
		{"Pacific/Chatham", time.Date(2024, 2, 29, 12, 45, 0, 0, time.UTC), 0},
		{"Antarctica/Troll", time.Date(2023, 7, 1, 0, 0, 0, 0, time.UTC), 0},
	}
	for _, extra := range extras {
		for _, zone := range zones {
			if zone.name != extra.zone {
				continue
			}
			res = append(res, timeDateRow(zone, "extra", extra.wall, extra.nsec))
		}
	}

	return res
}

func timeDateRow(zone recurrenceZone, group string, wall time.Time, nsec int) map[string]any {
	row := map[string]any{
		"zone":  zone.name,
		"group": group,
		"wall":  wallString(wall.Add(time.Duration(nsec))),
		"year":  wall.Year(),
		"month": int(wall.Month()),
		"day":   wall.Day(),
		"hour":  wall.Hour(),
		"min":   wall.Minute(),
		"sec":   wall.Second(),
		"nsec":  nsec,
	}

	probe(row, func() {
		got := time.Date(wall.Year(), wall.Month(), wall.Day(), wall.Hour(), wall.Minute(), wall.Second(), nsec, zone.loc)
		_, offset := got.Zone()

		row["unix_millis"] = got.UnixMilli()
		row["offset_seconds"] = offset
		// What the answer's wall clock actually is. When it differs from the requested `wall`,
		// the requested local time did not exist and this is where Go put it instead.
		row["resolved_wall"] = wallString(got)
		row["resolved_wall_matches_input"] = wallString(got) == wallString(wall.Add(time.Duration(nsec)))
	})

	return row
}

// --- ComputeNextScheduledAt ------------------------------------------------------------------

func recurrenceComputeNextAll(zones []recurrenceZone) []map[string]any {
	var res []map[string]any

	// The generated half: for every wall clock in every transition window, schedule a post
	// exactly seven local days earlier and ask for the next occurrence. The answer must land on
	// (Go's resolution of) the wall clock under test, so each row isolates one boundary.
	for _, zone := range zones {
		for i, tr := range zone.transitions {
			for _, w := range wallProbes(tr) {
				prior := w.AddDate(0, 0, -7)
				scheduledAt := time.Date(prior.Year(), prior.Month(), prior.Day(), prior.Hour(), prior.Minute(), prior.Second(), 0, zone.loc).UnixMilli()

				res = append(res, computeNextRow(map[string]any{
					"name":         fmt.Sprintf("%s#%d %s", zone.name, i, wallString(w)),
					"group":        "transition",
					"repeat_type":  model.ScheduledPostRepeatTypeWeekly,
					"timezone":     zone.name,
					"scheduled_at": scheduledAt,
					// One step: `next` is seven days after `scheduled_at`, which is already
					// after `now`.
					"now": scheduledAt,
				}))
			}
		}
	}

	// The hand-picked half. `scheduledAt` values are fixed constants; the interesting ones are
	// the loop-termination boundary and the multi-step cases, which the generated half never
	// exercises because it always takes exactly one step.
	const (
		// 2023-03-05T07:30:00Z — 02:30 EST, seven days before the New York gap.
		nyBeforeGap = int64(1677915000000)
		// 2023-11-05T05:30:00Z — 01:30 EDT, the earlier of the two 01:30s.
		nyInFold = int64(1699162200000)
		week     = int64(7 * 24 * 60 * 60 * 1000)
	)

	handPicked := []map[string]any{
		// The default arm. Every repeat type that is not "weekly" is an error, the empty one
		// included — so a non-recurring scheduled post cannot be asked for its next occurrence.
		{"name": "repeat type none", "group": "error", "repeat_type": model.ScheduledPostRepeatTypeNone, "timezone": "UTC", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},
		{"name": "repeat type daily", "group": "error", "repeat_type": "daily", "timezone": "UTC", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},
		{"name": "repeat type capitalised", "group": "error", "repeat_type": "Weekly", "timezone": "UTC", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},
		{"name": "repeat type quoted", "group": "error", "repeat_type": "a\"b\nc", "timezone": "UTC", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},

		// The timezone arm. `""` is UTC and `"Local"` is the server's own zone — both succeed in
		// Go, and BaseIsValid rejects both before they can get here. See [D-065].
		{"name": "timezone empty", "group": "timezone", "repeat_type": "weekly", "timezone": "", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},
		{"name": "timezone Local", "group": "timezone", "repeat_type": "weekly", "timezone": "Local", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},
		{"name": "timezone UTC", "group": "timezone", "repeat_type": "weekly", "timezone": "UTC", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},
		{"name": "timezone unknown", "group": "timezone", "repeat_type": "weekly", "timezone": "Nowhere/Nothing", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},
		{"name": "timezone path shaped", "group": "timezone", "repeat_type": "weekly", "timezone": "../etc/passwd", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},
		{"name": "timezone lowercase", "group": "timezone", "repeat_type": "weekly", "timezone": "america/new_york", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},

		// The loop. `for !next.After(now)` is strict, so the first candidate landing exactly on
		// `now` is rejected and a second week is added.
		{"name": "next lands exactly on now", "group": "loop", "repeat_type": "weekly", "timezone": "America/New_York", "scheduled_at": nyBeforeGap, "now": nyBeforeGap + week},
		{"name": "next lands one ms after now", "group": "loop", "repeat_type": "weekly", "timezone": "America/New_York", "scheduled_at": nyBeforeGap, "now": nyBeforeGap + week - 1},
		{"name": "next lands one ms before now", "group": "loop", "repeat_type": "weekly", "timezone": "America/New_York", "scheduled_at": nyBeforeGap, "now": nyBeforeGap + week + 1},
		{"name": "now is before scheduled_at", "group": "loop", "repeat_type": "weekly", "timezone": "America/New_York", "scheduled_at": nyBeforeGap, "now": nyBeforeGap - 10*week},
		// Four steps, crossing the gap on the second one — so the wall clock the third step adds
		// to is the one Go moved, not the one it was asked for. That drift is the whole reason
		// the loop has to re-resolve rather than add multiples of seven days.
		{"name": "four steps across the gap", "group": "loop", "repeat_type": "weekly", "timezone": "America/New_York", "scheduled_at": nyBeforeGap, "now": nyBeforeGap + 3*week + 1},
		{"name": "many steps across both boundaries", "group": "loop", "repeat_type": "weekly", "timezone": "America/New_York", "scheduled_at": nyBeforeGap, "now": nyBeforeGap + 40*week},
		{"name": "fold start, many steps", "group": "loop", "repeat_type": "weekly", "timezone": "America/New_York", "scheduled_at": nyInFold, "now": nyInFold + 20*week},

		// Fixed-offset zones cannot drift, so these pin the plain arithmetic.
		{"name": "kolkata plain", "group": "plain", "repeat_type": "weekly", "timezone": "Asia/Kolkata", "scheduled_at": nyBeforeGap, "now": nyBeforeGap},
		{"name": "kolkata sub-second", "group": "plain", "repeat_type": "weekly", "timezone": "Asia/Kolkata", "scheduled_at": nyBeforeGap + 123, "now": nyBeforeGap},
		{"name": "utc sub-second", "group": "plain", "repeat_type": "weekly", "timezone": "UTC", "scheduled_at": nyBeforeGap + 999, "now": nyBeforeGap},

		// Pre-epoch, so the scheduled instant sits in a zone record with an LMT offset that is
		// not a whole number of minutes, and `now` is far enough ahead to run the loop hard.
		{"name": "scheduled in 1850", "group": "plain", "repeat_type": "weekly", "timezone": "America/New_York", "scheduled_at": int64(-3781989000000), "now": int64(-3781989000000) + 3*week},
		{"name": "scheduled at the epoch", "group": "plain", "repeat_type": "weekly", "timezone": "America/New_York", "scheduled_at": int64(0), "now": int64(0)},
		{"name": "scheduled before the epoch, now after", "group": "plain", "repeat_type": "weekly", "timezone": "Europe/London", "scheduled_at": -week, "now": 4 * week},
	}
	for _, row := range handPicked {
		res = append(res, computeNextRow(row))
	}

	return res
}

func computeNextRow(row map[string]any) map[string]any {
	s := &model.ScheduledPost{
		RepeatType:     row["repeat_type"].(string),
		RepeatTimezone: row["timezone"].(string),
		ScheduledAt:    row["scheduled_at"].(int64),
	}
	now := row["now"].(int64)

	probe(row, func() {
		next, err := s.ComputeNextScheduledAt(now)
		if err != nil {
			row["ok"] = false
			row["err"] = err.Error()
			row["next"] = next
			return
		}
		row["ok"] = true
		row["err"] = nil
		row["next"] = next

		// `"Local"` resolves to the generating machine's own zone, so its answer is not a
		// property of Go and must not be written into a committed fixture — see [D-032]. What is
		// worth recording is that Go accepts the name at all, which `ok` already carries.
		if s.RepeatTimezone == "Local" {
			row["next"] = "<the generating host's zone>"
			return
		}

		// Diagnostics. `next_wall` is what makes a gap-resolution readable: when it differs from
		// the wall clock seven days after `scheduled_at`, Go moved the appointment.
		if loc, lerr := time.LoadLocation(s.RepeatTimezone); lerr == nil {
			row["next_wall"] = wallString(time.UnixMilli(next).In(loc))
			row["scheduled_wall"] = wallString(time.UnixMilli(s.ScheduledAt).In(loc))
			_, offset := time.UnixMilli(next).In(loc).Zone()
			row["next_offset_seconds"] = offset
		}
		row["is_recurring"] = s.IsRecurring()
	})

	return row
}
