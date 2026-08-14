package main

// Behavioural oracle for model/custom_status.go, written to fixtures/behaviour_custom_status.json.
//
// Two things here are unlike anything translated so far.
//
//  1. **CustomStatus.ExpiresAt is a `time.Time`, not epoch milliseconds.** Every other timestamp
//     in the model package is an int64 of milliseconds; this one marshals through Go's
//     time.Time, i.e. RFC 3339 with `.999999999` fractional seconds — trailing zeros stripped,
//     and the decimal point dropped entirely when the fraction is zero. chrono's default serde
//     impl uses SecondsFormat::AutoSi, which pads to 3/6/9 digits, so `.5` becomes `.500` and
//     the bytes drift. The field has no omitempty and the value is persisted inside
//     User.Props["customStatus"] as a marshalled string, so those bytes round-trip through the
//     database and are worth pinning exactly. `time_marshal` and `time_unmarshal` below are the
//     pin; they are about encoding/json and the time package, not about custom_status.go.
//
//  2. **PreSave and AreDurationAndExpirationTimeValid call time.Now().** A fixture cannot record
//     an absolute answer for those. Instead each case records an *offset* from now, which is a
//     deterministic input: the Rust test rebuilds `Utc::now() + offset` and must agree. Offsets
//     are whole hours so no plausible test runtime can flip one.
//
// The remaining functions are pure. Contains/Remove compare **marshalled bytes** rather than
// struct fields, so their corpora embed the statuses as Go-marshalled JSON — a wire drift and a
// logic drift then fail the same test.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strconv"
	"time"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeCustomStatusBehaviourFixture(outDir string) error {
	out := map[string]any{
		"time_marshal":            timeMarshalAll(),
		"time_unmarshal":          timeUnmarshalAll(),
		"time_unmarshal_sentinel": timeSentinelJSON(),
		"valid_durations":         validDurationsObserved(),
		"pre_save_text":           preSaveTextAll(),
		"pre_save_duration":       preSaveDurationAll(),
		"duration_and_expiry":     durationAndExpiryAll(),
		"rune_to_hex":             runeToHexAll(),
		"contains":                containsAll(),
		"add":                     addAll(),
		"remove":                  removeAll(),
		"user_custom_status":      userCustomStatusAll(),
		"set_custom_status":       setCustomStatusAll(),
		"clear_custom_status":     clearCustomStatusResult(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_custom_status.json"), append(blob, '\n'), 0o644)
}

// --- time.Time on the wire -------------------------------------------------------

type timeMarshalCase struct {
	Name string `json:"name"`
	// The instant is recorded as unix seconds + nanoseconds + zone offset rather than as a
	// formatted string, so the Rust side can rebuild it without parsing the very format under
	// test. Go's zero time is unix second -62135596800.
	UnixSeconds   int64  `json:"unix_seconds"`
	Nanos         int    `json:"nanos"`
	OffsetSeconds int    `json:"offset_seconds"`
	IsZero        bool   `json:"is_zero"`
	JSON          string `json:"json"`
	Err           string `json:"err"`
}

func timeMarshalCorpus() []struct {
	name string
	t    time.Time
} {
	utc := time.UTC
	ist := time.FixedZone("IST", 5*3600+1800)
	west := time.FixedZone("WEST", -8*3600)

	return []struct {
		name string
		t    time.Time
	}{
		{"zero", time.Time{}},
		{"epoch", time.Unix(0, 0).In(utc)},
		{"whole_second", time.Date(2026, 8, 14, 12, 0, 0, 0, utc)},
		// The fractional-second trimming ladder. Go writes the shortest form; a padding
		// encoder writes .500 / .120 / .100 and drifts on every one of these.
		{"half_second", time.Date(2026, 8, 14, 12, 0, 0, 500000000, utc)},
		{"millis_120", time.Date(2026, 8, 14, 12, 0, 0, 120000000, utc)},
		{"millis_100", time.Date(2026, 8, 14, 12, 0, 0, 100000000, utc)},
		{"millis_123", time.Date(2026, 8, 14, 12, 0, 0, 123000000, utc)},
		{"micros_123456", time.Date(2026, 8, 14, 12, 0, 0, 123456000, utc)},
		{"nanos_123456789", time.Date(2026, 8, 14, 12, 0, 0, 123456789, utc)},
		{"one_nano", time.Date(2026, 8, 14, 12, 0, 0, 1, utc)},
		{"ten_millis", time.Date(2026, 8, 14, 12, 0, 0, 10000000, utc)},
		{"nanos_trailing_zero", time.Date(2026, 8, 14, 12, 0, 0, 123456780, utc)},
		// Non-UTC zones: Go re-emits the offset it holds, it does not normalise to Z.
		{"positive_offset", time.Date(2026, 8, 14, 12, 0, 0, 0, ist)},
		{"negative_offset", time.Date(2026, 8, 14, 12, 0, 0, 0, west)},
		{"offset_with_nanos", time.Date(2026, 8, 14, 12, 0, 0, 250000000, ist)},
		// Before the epoch, and either side of the [0,9999] year range MarshalJSON enforces.
		{"pre_epoch", time.Date(1969, 7, 20, 20, 17, 40, 0, utc)},
		{"year_9999", time.Date(9999, 12, 31, 23, 59, 59, 999999999, utc)},
		{"year_10000", time.Date(10000, 1, 1, 0, 0, 0, 0, utc)},
		{"year_negative", time.Date(-1, 1, 1, 0, 0, 0, 0, utc)},
	}
}

func timeMarshalAll() []timeMarshalCase {
	var res []timeMarshalCase
	for _, c := range timeMarshalCorpus() {
		_, offset := c.t.Zone()
		out := timeMarshalCase{
			Name:          c.name,
			UnixSeconds:   c.t.Unix(),
			Nanos:         c.t.Nanosecond(),
			OffsetSeconds: offset,
			IsZero:        c.t.IsZero(),
		}
		blob, err := json.Marshal(c.t)
		if err != nil {
			out.Err = err.Error()
		} else {
			out.JSON = string(blob)
		}
		res = append(res, out)
	}
	return res
}

type timeUnmarshalCase struct {
	In  string `json:"in"`
	OK  bool   `json:"ok"`
	Out string `json:"out"`
}

// timeSentinel is what the receiver holds *before* unmarshalling, so the corpus can show which
// inputs leave it untouched. Go's Time.UnmarshalJSON returns early on `null` without writing,
// which a Rust Deserialize impl has to reproduce deliberately — serde rejects null by default.
var timeSentinel = time.Date(2001, 2, 3, 4, 5, 6, 700000000, time.UTC)

func timeSentinelJSON() string {
	blob, err := json.Marshal(timeSentinel)
	if err != nil {
		panic(err)
	}
	return string(blob)
}

// The inputs are raw JSON tokens, quotes included, so malformed and non-string forms are
// reachable too.
var timeUnmarshalCorpus = []string{
	`"2026-08-14T12:00:00Z"`,
	`"2026-08-14T12:00:00.5Z"`,
	`"2026-08-14T12:00:00.500Z"`,
	`"2026-08-14T12:00:00.123456789Z"`,
	`"2026-08-14T12:00:00.1234567891Z"`,
	`"2026-08-14T12:00:00+05:30"`,
	`"2026-08-14T12:00:00-08:00"`,
	`"2026-08-14T12:00:00+00:00"`,
	`"2026-08-14T12:00:00-00:00"`,
	`"0001-01-01T00:00:00Z"`,
	`"1969-07-20T20:17:40Z"`,
	// case: RFC 3339 says the separators are case-insensitive; Go's parser has opinions.
	`"2026-08-14t12:00:00z"`,
	`"2026-08-14T12:00:00z"`,
	// shapes that are not RFC 3339
	`"2026-08-14T12:00:00"`,
	`"2026-08-14 12:00:00Z"`,
	`"2026-08-14"`,
	`"12:00:00Z"`,
	`"2026-08-14T12:00:00.Z"`,
	`"2026-8-14T12:00:00Z"`,
	`"2026-08-14T12:00:00+0530"`,
	`"2026-08-14T12:00:00Z07:00"`,
	`"2026-13-14T12:00:00Z"`,
	`"2026-02-30T12:00:00Z"`,
	`"2026-08-14T25:00:00Z"`,
	`"2024-02-29T12:00:00Z"`,
	`"2023-02-29T12:00:00Z"`,
	`"2026-08-14T23:59:60Z"`,
	`"2026-08-14T12:00:00.000000000Z"`,
	// Offsets the strict parser does not range-check: it reads two digits either side of the
	// colon and multiplies out, so these exceed what chrono's FixedOffset can hold.
	`"2026-08-14T12:00:00+23:59"`,
	`"2026-08-14T12:00:00+99:99"`,
	`"2026-08-14T12:00:00-99:99"`,
	// A signed year field — Go reads s[0:4] with atoi, which accepts a leading sign.
	`"-026-08-14T12:00:00Z"`,
	`"+026-08-14T12:00:00Z"`,
	`""`,
	`"   "`,
	// non-string tokens
	`null`,
	`0`,
	`1700000000000`,
	`true`,
	`{}`,
	`[]`,
}

func timeUnmarshalAll() []timeUnmarshalCase {
	res := make([]timeUnmarshalCase, 0, len(timeUnmarshalCorpus))
	for _, in := range timeUnmarshalCorpus {
		t := timeSentinel
		c := timeUnmarshalCase{In: in}
		if err := json.Unmarshal([]byte(in), &t); err != nil {
			c.OK = false
		} else {
			c.OK = true
			blob, err := json.Marshal(t)
			if err != nil {
				panic(err)
			}
			c.Out = string(blob)
		}
		res = append(res, c)
	}
	return res
}

// --- validCustomStatusDuration --------------------------------------------------

// The map is unexported, so its membership is observed through the only function that reads it,
// with a non-expired ExpiresAt so the second branch is the one that decides. Note "" comes back
// true without being in the map: it is caught by the *first* branch of
// AreDurationAndExpirationTimeValid, which special-cases an absent duration.
func validDurationsObserved() map[string]bool {
	candidates := []string{
		"thirty_minutes", "one_hour", "four_hours", "today", "this_week", "date_and_time",
		"", "one_minute", "tomorrow", "this_month", "forever", "DATE_AND_TIME", "date and time",
	}
	future := time.Now().Add(24 * time.Hour)
	res := make(map[string]bool, len(candidates))
	for _, d := range candidates {
		cs := &model.CustomStatus{Duration: d, ExpiresAt: future}
		res[d] = cs.AreDurationAndExpirationTimeValid()
	}
	return res
}

// --- PreSave --------------------------------------------------------------------

type preSaveTextCase struct {
	In  string `json:"in"`
	Out string `json:"out"`
}

// PreSave truncates Text to 100 *runes*, so every case that matters is non-ASCII. The emoji
// entries also cover the surrogate-pair-in-UTF-16 shape clients send.
var preSaveTextCorpus = []string{
	"",
	"hello",
	repeat("a", 99),
	repeat("a", 100),
	repeat("a", 101),
	repeat("a", 500),
	repeat("é", 100),
	repeat("é", 101),
	repeat("☃", 101),          // snowman, 3 bytes
	repeat("\U0001F600", 101), // grinning face, 4 bytes, 1 rune
	repeat("\U0001F600", 100),
	repeat("á", 101),                      // 'a' + combining acute: 2 runes each, so 202 runes
	"\ufeffleading bom" + repeat("x", 200), // BOM as an escape: a literal one is a Go compile error
}

func preSaveTextAll() []preSaveTextCase {
	res := make([]preSaveTextCase, 0, len(preSaveTextCorpus))
	for _, in := range preSaveTextCorpus {
		cs := &model.CustomStatus{Text: in}
		cs.PreSave()
		res = append(res, preSaveTextCase{In: in, Out: cs.Text})
	}
	return res
}

type expiryCase struct {
	Duration string `json:"duration"`
	// Exactly one of these describes ExpiresAt: Zero means time.Time{}, otherwise the value is
	// now + OffsetHours. Hours, so no test runtime can straddle the boundary.
	Zero        bool  `json:"expires_at_zero"`
	OffsetHours int64 `json:"expires_at_offset_hours"`
	Out         any   `json:"out"`
}

func expiryCorpus() []struct {
	duration string
	zero     bool
	offset   int64
} {
	var res []struct {
		duration string
		zero     bool
		offset   int64
	}
	durations := []string{"", "date_and_time", "one_hour", "thirty_minutes", "four_hours", "today", "this_week", "bogus", "DATE_AND_TIME"}
	for _, d := range durations {
		res = append(res, struct {
			duration string
			zero     bool
			offset   int64
		}{d, true, 0})
		for _, off := range []int64{-8760, -24, -1, 1, 24, 8760} {
			res = append(res, struct {
				duration string
				zero     bool
				offset   int64
			}{d, false, off})
		}
	}
	return res
}

func expiresAtFor(zero bool, offsetHours int64) time.Time {
	if zero {
		return time.Time{}
	}
	return time.Now().Add(time.Duration(offsetHours) * time.Hour)
}

// preSaveDurationAll records the *only* thing PreSave does to Duration: promote an empty one to
// "date_and_time" when the expiry has not already passed. Note the zero time counts as passed.
func preSaveDurationAll() []expiryCase {
	var res []expiryCase
	for _, c := range expiryCorpus() {
		cs := &model.CustomStatus{Duration: c.duration, ExpiresAt: expiresAtFor(c.zero, c.offset)}
		cs.PreSave()
		res = append(res, expiryCase{Duration: c.duration, Zero: c.zero, OffsetHours: c.offset, Out: cs.Duration})
	}
	return res
}

func durationAndExpiryAll() []expiryCase {
	var res []expiryCase
	for _, c := range expiryCorpus() {
		cs := &model.CustomStatus{Duration: c.duration, ExpiresAt: expiresAtFor(c.zero, c.offset)}
		res = append(res, expiryCase{
			Duration:    c.duration,
			Zero:        c.zero,
			OffsetHours: c.offset,
			Out:         cs.AreDurationAndExpirationTimeValid(),
		})
	}
	return res
}

// --- RuneToHexadecimalString ------------------------------------------------------

func runeToHexAll() map[string]string {
	runes := []rune{
		0, 1, 0xF, 0x10, 'a', 'Z', '0', 0xFF, 0x100, 0xFFF, 0x1000, 0xFFFF,
		0x10000, 0x1F600, 0x10FFFF, 0x2603, 0x301, 0xFEFF,
	}
	res := make(map[string]string, len(runes))
	for _, r := range runes {
		// Key by the decimal code point: a rune key would be unreadable and, for the
		// unassigned ones, unrepresentable in a JSON object key without escaping.
		res[strconv.Itoa(int(r))] = model.RuneToHexadecimalString(r)
	}
	return res
}

// --- RecentCustomStatuses ----------------------------------------------------------

// Statuses are embedded as Go-marshalled JSON so a wire drift fails these tests too.
type recentCase struct {
	Name  string            `json:"name"`
	List  []json.RawMessage `json:"list"`
	Arg   json.RawMessage   `json:"arg"`
	Out   json.RawMessage   `json:"out"`
	Found bool              `json:"found"`
}

func marshalStatus(cs model.CustomStatus) json.RawMessage {
	blob, err := json.Marshal(cs)
	if err != nil {
		panic(err)
	}
	return blob
}

func marshalList(rcs model.RecentCustomStatuses) []json.RawMessage {
	out := make([]json.RawMessage, 0, len(rcs))
	for _, cs := range rcs {
		out = append(out, marshalStatus(cs))
	}
	return out
}

// A fixed instant, so nothing in these corpora depends on the clock.
var recentTime = time.Date(2026, 8, 14, 12, 0, 0, 0, time.UTC)

func status(emoji, text string) model.CustomStatus {
	return model.CustomStatus{Emoji: emoji, Text: text, Duration: "date_and_time", ExpiresAt: recentTime}
}

func recentCorpus() []struct {
	name string
	list model.RecentCustomStatuses
	arg  model.CustomStatus
} {
	five := model.RecentCustomStatuses{
		status("a", "one"), status("b", "two"), status("c", "three"),
		status("d", "four"), status("e", "five"),
	}
	return []struct {
		name string
		list model.RecentCustomStatuses
		arg  model.CustomStatus
	}{
		{"empty_list", model.RecentCustomStatuses{}, status("a", "one")},
		{"present_exact", five, status("c", "three")},
		// Same text, different emoji: Contains says no (byte equality), Add still dedups (Text only).
		{"same_text_other_emoji", five, status("z", "three")},
		{"same_emoji_other_text", five, status("c", "zzz")},
		{"absent", five, status("z", "zzz")},
		{"arg_fully_empty", five, model.CustomStatus{}},
		{"arg_empty_emoji_and_text", five, model.CustomStatus{Duration: "date_and_time", ExpiresAt: recentTime}},
		{"arg_empty_text_only", five, status("a", "")},
		{"arg_empty_emoji_only", five, status("", "one")},
		{"list_at_cap", five, status("f", "six")},
		{"list_over_cap", model.RecentCustomStatuses{
			status("a", "one"), status("b", "two"), status("c", "three"),
			status("d", "four"), status("e", "five"), status("f", "six"),
		}, status("g", "seven")},
		{"duplicate_texts_in_list", model.RecentCustomStatuses{
			status("a", "dup"), status("b", "dup"), status("c", "keep"),
		}, status("z", "dup")},
		{"single", model.RecentCustomStatuses{status("a", "one")}, status("a", "one")},
		{"differs_only_in_expiry", model.RecentCustomStatuses{
			{Emoji: "a", Text: "one", Duration: "date_and_time", ExpiresAt: recentTime.Add(time.Hour)},
		}, status("a", "one")},
	}
}

func containsAll() []recentCase {
	var res []recentCase
	for _, c := range recentCorpus() {
		// Copy the list: Contains does not mutate, but Add and Remove below do, and sharing a
		// backing array between corpora would let one case contaminate the next.
		list := append(model.RecentCustomStatuses{}, c.list...)
		arg := c.arg
		found, err := list.Contains(&arg)
		if err != nil {
			panic(err)
		}
		res = append(res, recentCase{Name: c.name, List: marshalList(c.list), Arg: marshalStatus(c.arg), Found: found})
	}
	return res
}

func addAll() []recentCase {
	var res []recentCase
	for _, c := range recentCorpus() {
		list := append(model.RecentCustomStatuses{}, c.list...)
		arg := c.arg
		out := list.Add(&arg)
		blob, err := json.Marshal(marshalList(out))
		if err != nil {
			panic(err)
		}
		res = append(res, recentCase{Name: c.name, List: marshalList(c.list), Arg: marshalStatus(c.arg), Out: blob})
	}
	return res
}

func removeAll() []recentCase {
	var res []recentCase
	for _, c := range recentCorpus() {
		list := append(model.RecentCustomStatuses{}, c.list...)
		arg := c.arg
		out, err := list.Remove(&arg)
		if err != nil {
			panic(err)
		}
		blob, jsonErr := json.Marshal(marshalList(out))
		if jsonErr != nil {
			panic(jsonErr)
		}
		res = append(res, recentCase{Name: c.name, List: marshalList(c.list), Arg: marshalStatus(c.arg), Out: blob})
	}
	return res
}

// --- user.go's custom-status accessors ---------------------------------------

// These five methods live in user.go, not custom_status.go, but everything interesting about
// them is CustomStatus decoding, so their corpus belongs here.
//
// The trap is that GetCustomStatus **discards the unmarshal error**:
//
//	data := u.Props[UserPropsKeyCustomStatus]
//	_ = json.Unmarshal([]byte(data), &o)
//
// Go's encoding/json is not all-or-nothing the way serde_json is. A syntax error writes nothing,
// but a *type* error (`{"emoji": 123}`) leaves the fields decoded before it in place and reports
// the failure at the end — which is then thrown away, so a caller sees a partially populated
// status. A failing Unmarshaler (a malformed expires_at) aborts harder, keeping only what was
// decoded before that key. serde_json returns Err and no value in every one of those cases, so
// the corpus below is what decides whether `.ok()` is a faithful port or a silent divergence.
type userCustomStatusCase struct {
	Name string `json:"name"`
	// The raw string held in User.Props["customStatus"]. PropPresent distinguishes an absent
	// key from one holding "" — ValidateCustomStatus reads both, and treats them alike.
	Prop        string `json:"prop"`
	PropPresent bool   `json:"prop_present"`
	// GetCustomStatus's return value, re-marshalled; JSON null when it returned nil.
	Get      json.RawMessage `json:"get"`
	Validate bool            `json:"validate"`
}

var userCustomStatusCorpus = []struct {
	name    string
	prop    string
	present bool
}{
	{"absent", "", false},
	{"empty_string", "", true},
	{"null_literal", "null", true},
	{"empty_object", "{}", true},
	{"complete", `{"emoji":"a","text":"b","duration":"date_and_time","expires_at":"2026-08-14T12:00:00Z"}`, true},
	{"offset_preserved", `{"emoji":"a","text":"b","duration":"date_and_time","expires_at":"2026-08-14T12:00:00+05:30"}`, true},
	{"partial_keys", `{"emoji":"a"}`, true},
	{"unknown_key", `{"emoji":"a","nope":1}`, true},
	{"expires_at_null", `{"emoji":"a","expires_at":null}`, true},
	// Type error on the first key: Go keeps decoding and populates the rest.
	{"type_error_first", `{"emoji":123,"text":"kept"}`, true},
	// Type error on the last key: everything before it survives.
	{"type_error_last", `{"emoji":"kept","text":123}`, true},
	// A failing Unmarshaler aborts the object, so keys after it are dropped.
	{"bad_expires_at_middle", `{"emoji":"kept","expires_at":"garbage","text":"dropped?"}`, true},
	{"bad_expires_at_last", `{"emoji":"kept","text":"kept","expires_at":"garbage"}`, true},
	// Syntax errors write nothing at all.
	{"truncated", `{"emoji":"a"`, true},
	{"garbage", "garbage", true},
	{"trailing_data", `{"emoji":"a"}{"emoji":"b"}`, true},
	{"leading_whitespace", `   {"emoji":"a"}   `, true},
	// Wrong JSON shapes.
	{"json_string", `"a string"`, true},
	{"json_array", "[]", true},
	{"json_number", "0", true},
	{"json_true", "true", true},
}

func userCustomStatusAll() []userCustomStatusCase {
	res := make([]userCustomStatusCase, 0, len(userCustomStatusCorpus))
	for _, c := range userCustomStatusCorpus {
		u := &model.User{}
		if c.present {
			u.Props = model.StringMap{model.UserPropsKeyCustomStatus: c.prop}
		}

		got := u.GetCustomStatus()
		blob, err := json.Marshal(got)
		if err != nil {
			panic(err)
		}
		res = append(res, userCustomStatusCase{
			Name:        c.name,
			Prop:        c.prop,
			PropPresent: c.present,
			Get:         blob,
			Validate:    u.ValidateCustomStatus(),
		})
	}
	return res
}

// setCustomStatusAll records the exact string SetCustomStatus writes into Props. Note it stores
// Go's marshalling of the *pointer*, so a nil status is stored as the four bytes "null" rather
// than being an error or a no-op.
func setCustomStatusAll() map[string]string {
	cases := map[string]*model.CustomStatus{
		"nil":      nil,
		"zero":     {},
		"complete": {Emoji: "a", Text: "b", Duration: "date_and_time", ExpiresAt: recentTime},
		"html":     {Emoji: "<b>", Text: "a&b", Duration: "date_and_time", ExpiresAt: recentTime},
	}
	res := make(map[string]string, len(cases))
	for name, cs := range cases {
		u := &model.User{}
		if err := u.SetCustomStatus(cs); err != nil {
			panic(err)
		}
		res[name] = u.Props[model.UserPropsKeyCustomStatus]
	}
	return res
}

// clearCustomStatusResult pins that Clear writes an empty string rather than deleting the key.
func clearCustomStatusResult() map[string]any {
	u := &model.User{}
	u.ClearCustomStatus()
	value, exists := u.Props[model.UserPropsKeyCustomStatus]
	return map[string]any{"value": value, "key_exists": exists}
}
