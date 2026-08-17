package main

// Behavioural oracle for model/channel_member_history.go and
// model/channel_member_history_result.go, written to
// fixtures/behaviour_channel_member_history.json.
//
// Two structs, no methods, no constructors, and — the reason they are worth an oracle at all —
// **not one `json:` tag between them**:
//
//	type ChannelMemberHistory struct {
//	    ChannelId string
//	    UserId    string
//	    JoinTime  int64
//	    LeaveTime *int64
//	}
//
//	type ChannelMemberHistoryResult struct {
//	    ChannelId string
//	    UserId    string
//	    JoinTime  int64
//	    LeaveTime *int64
//
//	    // these two fields are never set in the database - when we SELECT, we join on Users to get them
//	    UserEmail    string `db:"Email"`
//	    Username     string
//	    IsBot        bool
//	    UserDeleteAt int64
//	}
//
// So every wire key is the Go **field name verbatim**, capitalisation included — the same shape
// as `wrangler.go`, and the second instance of it in the tree. That is the whole content of the
// port, and it is exactly the kind of thing a translator writes as snake_case out of habit, so
// the key list is read off the struct tags with reflection rather than transcribed.
//
// Two traps beyond the casing:
//
//   - **`UserEmail` carries `db:"Email"` and no json tag.** `encoding/json` does not look at
//     `db`, so the wire key is `UserEmail` while the column is `Email`. A port that copied the
//     tag it could see would rename the field on the wire.
//   - **`LeaveTime` is a `*int64` with no `omitempty`**, so nil is `null` on the wire and the key
//     is always present. It is the only nillable field in either struct, and it is the one that
//     distinguishes a member who is still in the channel from one who has left.
//
// The comment above the second group says "these two fields" and there are **four** of them.
// Recorded rather than corrected — it is upstream's, and it is a hint that the group grew without
// the comment being updated, which is worth knowing if a fifth ever appears.
//
// Determinism: fixed values only. No NewId, no time.Now — see [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeChannelMemberHistoryBehaviourFixture(outDir string) error {
	out := map[string]any{
		"history_keys":      expectedKeys(reflect.TypeOf(model.ChannelMemberHistory{})),
		"result_keys":       expectedKeys(reflect.TypeOf(model.ChannelMemberHistoryResult{})),
		"history_wire":      cmhHistoryWireAll(),
		"result_wire":       cmhResultWireAll(),
		"leave_time_decode": cmhLeaveTimeAll(),
		"key_casing_decode": cmhKeyCasingAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_channel_member_history.json"), append(blob, '\n'), 0o644)
}

// --- ChannelMemberHistory ------------------------------------------------------------------------

// cmhHistoryWireAll marshals values rather than decoding literals, so the recorded document is
// the one a Go server emits — every key present, in declaration order. See [D-043] and the
// channel_data.go note for why hand-written partial documents are the wrong input here.
func cmhHistoryWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.ChannelMemberHistory
	}{
		{"zero", model.ChannelMemberHistory{}},
		{"still_present", model.ChannelMemberHistory{
			ChannelId: "qr6kf7ztp7yifxt4wm5xn51bke",
			UserId:    "6bdz674pgq767e4jx75w4pf57a",
			JoinTime:  1700000000000,
		}},
		{"left", model.ChannelMemberHistory{
			ChannelId: "qr6kf7ztp7yifxt4wm5xn51bke",
			UserId:    "6bdz674pgq767e4jx75w4pf57a",
			JoinTime:  1700000000000,
			LeaveTime: model.NewPointer(int64(1700000060000)),
		}},
		// A non-nil pointer to zero is a third state, distinct from nil and from a real time.
		{"left_at_zero", model.ChannelMemberHistory{
			JoinTime:  1,
			LeaveTime: model.NewPointer(int64(0)),
		}},
		{"negative_times", model.ChannelMemberHistory{
			JoinTime:  -1,
			LeaveTime: model.NewPointer(int64(-2)),
		}},
		{"int64_bounds", model.ChannelMemberHistory{
			JoinTime:  -9223372036854775808,
			LeaveTime: model.NewPointer(int64(9223372036854775807)),
		}},
		// Neither string field is validated, so anything round-trips — including characters Go's
		// encoder HTML-escapes.
		{"escaped", model.ChannelMemberHistory{
			ChannelId: "<a>&b",
			UserId:    "c d",
		}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			row["json"] = mustMarshal(c.in)
			row["leave_time_nil"] = c.in.LeaveTime == nil
		})
		res = append(res, row)
	}
	return res
}

// --- ChannelMemberHistoryResult --------------------------------------------------------------------

func cmhResultWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.ChannelMemberHistoryResult
	}{
		{"zero", model.ChannelMemberHistoryResult{}},
		{"joined_only", model.ChannelMemberHistoryResult{
			ChannelId: "qr6kf7ztp7yifxt4wm5xn51bke",
			UserId:    "6bdz674pgq767e4jx75w4pf57a",
			JoinTime:  1700000000000,
			UserEmail: "user@example.com",
			Username:  "parity-user",
		}},
		{"left", model.ChannelMemberHistoryResult{
			ChannelId: "qr6kf7ztp7yifxt4wm5xn51bke",
			UserId:    "6bdz674pgq767e4jx75w4pf57a",
			JoinTime:  1700000000000,
			LeaveTime: model.NewPointer(int64(1700000060000)),
			UserEmail: "user@example.com",
			Username:  "parity-user",
		}},
		{"bot", model.ChannelMemberHistoryResult{
			UserId:    "6bdz674pgq767e4jx75w4pf57a",
			JoinTime:  1,
			UserEmail: "bot@example.com",
			Username:  "a-bot",
			IsBot:     true,
		}},
		{"deleted_user", model.ChannelMemberHistoryResult{
			UserId:       "6bdz674pgq767e4jx75w4pf57a",
			JoinTime:     1,
			Username:     "gone",
			UserDeleteAt: 1700000000000,
		}},
		// Every field non-zero at once, which is what pins the emission order.
		{"all_set", model.ChannelMemberHistoryResult{
			ChannelId:    "qr6kf7ztp7yifxt4wm5xn51bke",
			UserId:       "6bdz674pgq767e4jx75w4pf57a",
			JoinTime:     100,
			LeaveTime:    model.NewPointer(int64(200)),
			UserEmail:    "user@example.com",
			Username:     "parity-user",
			IsBot:        true,
			UserDeleteAt: 300,
		}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			row["json"] = mustMarshal(c.in)
			row["leave_time_nil"] = c.in.LeaveTime == nil
		})
		res = append(res, row)
	}
	return res
}

// --- the nillable scalar ---------------------------------------------------------------------------

// cmhLeaveTimeAll drives the decode side of the only pointer in either struct. `null` is the
// interesting one: it is a legal value for a `*int64` and Go leaves the pointer nil, where the
// same JSON into a non-pointer `int64` would also be accepted but would leave a zero. The two
// states are distinguishable here and are not in `JoinTime`.
func cmhLeaveTimeAll() []map[string]any {
	values := []struct{ name, raw string }{
		{"integer", `1700000060000`},
		{"zero", `0`},
		{"negative", `-1`},
		{"null", `null`},
		{"max_int64", `9223372036854775807`},
		{"max_int64_plus_one", `9223372036854775808`},
		{"fractional_but_whole", `1.0`},
		{"fractional", `1.5`},
		{"quoted_number", `"1"`},
		{"bool", `true`},
	}

	var res []map[string]any
	for _, v := range values {
		doc := `{"ChannelId":"c1","UserId":"u1","JoinTime":100,"LeaveTime":` + v.raw + `}`
		row := map[string]any{"name": v.name, "raw": v.raw, "in": doc}
		probe(row, func() {
			var out model.ChannelMemberHistory
			err := json.Unmarshal([]byte(doc), &out)
			row["ok"] = err == nil
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}
			row["leave_time_nil"] = out.LeaveTime == nil
			if out.LeaveTime != nil {
				row["leave_time"] = *out.LeaveTime
			}
			// Recorded either way: a failure on LeaveTime leaves the earlier fields populated.
			row["join_time_after"] = out.JoinTime
			row["json_after"] = mustMarshal(out)
		})
		res = append(res, row)
	}
	return res
}

// --- the casing ------------------------------------------------------------------------------------

// cmhKeyCasingAll is the trap the missing json tags create. Go's decoder matches the exact field
// name first and falls back to a case-insensitive match, so all four spellings below populate the
// field there — while a serde port matching one `rename` string accepts exactly one of them.
// Recording all four turns "which spelling is canonical" into a measurement and makes the size of
// the [D-040] exposure visible for a type where *every* key is affected.
func cmhKeyCasingAll() []map[string]any {
	spellings := []struct{ name, key string }{
		{"declared", "ChannelId"},
		{"snake_case", "channel_id"},
		{"lowercase", "channelid"},
		{"uppercase", "CHANNELID"},
		{"mixed", "cHaNnElId"},
		// A near miss: Go's fallback is case-insensitive, not punctuation-insensitive.
		{"hyphenated", "channel-id"},
	}

	var res []map[string]any
	for _, s := range spellings {
		doc := `{"` + s.key + `":"c1","JoinTime":100}`
		row := map[string]any{"name": s.name, "key": s.key, "in": doc}
		probe(row, func() {
			var out model.ChannelMemberHistory
			err := json.Unmarshal([]byte(doc), &out)
			row["ok"] = err == nil
			row["channel_id"] = out.ChannelId
			row["populated"] = out.ChannelId != ""
		})
		res = append(res, row)
	}
	return res
}
