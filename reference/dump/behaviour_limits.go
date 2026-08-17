package main

// Behavioural oracle for model/limits.go, written to fixtures/behaviour_limits.json.
//
// One struct, seven `int64` fields, no methods, no pointers, no `omitempty`. The types are the
// least interesting thing about it; the **tags** are the content:
//
//	MaxUsersLimit           int64 `json:"maxUsersLimit"`
//	SingleChannelGuestCount int64 `json:"singleChannelGuestCount"`
//	LastAccessiblePostTime  int64 `json:"lastAccessiblePostTime"`
//
// Every key is **camelCase**. That is a third naming convention in the ported tree, after
// snake_case (everything else with tags) and `wrangler.go`'s tagless PascalCase — and it is
// exactly the sort of thing a translator normalises to `max_users_limit` without noticing,
// because sixty other files in a row were snake_case. So the key list is read off the struct tags
// with reflection and asserted in order, rather than transcribed.
//
// It also interacts with [D-040] in a way worth measuring rather than assuming: Go folds case
// against the **effective name**, which here is the camelCase tag. So `maxuserslimit` and
// `MAXUSERSLIMIT` populate the field in Go, and so does `MaxUsersLimit` — the Go field name
// happens to be a case-variant of its own tag. `max_users_limit`, the spelling a Rust port would
// naturally invent, populates **nothing** on either side.
//
// # Four of the seven fields carry a sentinel, and it is zero
//
// Go's comments say so for two of them: `PostHistoryLimit` is "0 if no limits" and
// `LastAccessiblePostTime` is "0 if no limits reached". So a zero is *meaningful* here rather
// than merely absent — and since nothing carries `omitempty`, every key is transmitted and the
// distinction survives. The corpus drives an all-zero document explicitly for that reason.
//
// Determinism: fixed values only. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"math"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeLimitsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"keys":       expectedKeys(reflect.TypeOf(model.ServerLimits{})),
		"wire":       limitsWireAll(),
		"key_casing": limitsKeyCasingAll(),
		"decode":     limitsDecodeAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_limits.json"), append(blob, '\n'), 0o644)
}

// --- the wire format ------------------------------------------------------------------------------

func limitsWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.ServerLimits
	}{
		// The sentinel document: every field zero, which for four of them means "no limit"
		// rather than "unset". No omitempty, so all seven keys are still transmitted.
		{"all_zero", model.ServerLimits{}},
		{"typical", model.ServerLimits{
			MaxUsersLimit:           10000,
			MaxUsersHardLimit:       11000,
			ActiveUserCount:         8423,
			SingleChannelGuestCount: 12,
			SingleChannelGuestLimit: 50,
			PostHistoryLimit:        5000,
			LastAccessiblePostTime:  1700000000000,
		}},
		// Over the soft limit but under the hard one: nothing validates the relationship.
		{"over_soft_limit", model.ServerLimits{
			MaxUsersLimit:     100,
			MaxUsersHardLimit: 120,
			ActiveUserCount:   110,
		}},
		// Hard limit below the soft one, which is nonsense and representable.
		{"inverted_limits", model.ServerLimits{MaxUsersLimit: 100, MaxUsersHardLimit: 10}},
		{"unlimited_history", model.ServerLimits{ActiveUserCount: 5, PostHistoryLimit: 0}},
		{"negative", model.ServerLimits{MaxUsersLimit: -1, LastAccessiblePostTime: -1}},
		{"int64_bounds", model.ServerLimits{
			MaxUsersLimit:          math.MaxInt64,
			LastAccessiblePostTime: math.MinInt64,
		}},
		// One field at a time, so a mis-tagged key shows up as the wrong key carrying the value
		// rather than as a document that happens to still parse.
		{"only_max_users_limit", model.ServerLimits{MaxUsersLimit: 1}},
		{"only_max_users_hard_limit", model.ServerLimits{MaxUsersHardLimit: 2}},
		{"only_active_user_count", model.ServerLimits{ActiveUserCount: 3}},
		{"only_single_channel_guest_count", model.ServerLimits{SingleChannelGuestCount: 4}},
		{"only_single_channel_guest_limit", model.ServerLimits{SingleChannelGuestLimit: 5}},
		{"only_post_history_limit", model.ServerLimits{PostHistoryLimit: 6}},
		{"only_last_accessible_post_time", model.ServerLimits{LastAccessiblePostTime: 7}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() { row["json"] = mustMarshal(c.in) })
		res = append(res, row)
	}
	return res
}

// --- the casing ------------------------------------------------------------------------------------

// limitsKeyCasingAll drives seven spellings of one camelCase key. The point is the snake_case row:
// `max_users_limit` is the spelling a port would invent by habit, and it populates the field on
// **neither** side — so a mis-tagged Rust field would not be caught by a round trip through its
// own serializer, only by comparing against Go's key list.
//
// The rest measures [D-040]'s reach for a camelCase tag, which is wider than for a snake_case one:
// the Go *field name* `MaxUsersLimit` is itself a case-variant of the tag `maxUsersLimit`, so both
// spellings work in Go where a snake_case tag admits no PascalCase spelling at all.
func limitsKeyCasingAll() []map[string]any {
	spellings := []struct{ name, key string }{
		{"declared_tag", "maxUsersLimit"},
		{"go_field_name", "MaxUsersLimit"},
		{"all_lower", "maxuserslimit"},
		{"all_upper", "MAXUSERSLIMIT"},
		{"mixed", "mAxUsErSlImIt"},
		// The habit spelling. Neither side accepts it.
		{"snake_case", "max_users_limit"},
		{"kebab_case", "max-users-limit"},
	}

	var res []map[string]any
	for _, s := range spellings {
		doc := `{"` + s.key + `":42}`
		row := map[string]any{"name": s.name, "key": s.key, "in": doc}
		probe(row, func() {
			var out model.ServerLimits
			err := json.Unmarshal([]byte(doc), &out)
			row["ok"] = err == nil
			row["value"] = out.MaxUsersLimit
			row["populated"] = out.MaxUsersLimit != 0
		})
		res = append(res, row)
	}
	return res
}

// --- the decode -------------------------------------------------------------------------------------

func limitsDecodeAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"empty", `{}`},
		{"partial", `{"activeUserCount":5}`},
		{"full", `{"maxUsersLimit":1,"maxUsersHardLimit":2,"activeUserCount":3,` +
			`"singleChannelGuestCount":4,"singleChannelGuestLimit":5,"postHistoryLimit":6,` +
			`"lastAccessiblePostTime":7}`},
		{"unknown_key", `{"nope":1}`},
		{"null_int", `{"activeUserCount":null}`},
		{"fractional", `{"activeUserCount":1.5}`},
		{"fractional_but_whole", `{"activeUserCount":1.0}`},
		{"exponent", `{"activeUserCount":1e3}`},
		{"quoted_number", `{"activeUserCount":"5"}`},
		{"bool", `{"activeUserCount":true}`},
		{"max_int64", `{"activeUserCount":9223372036854775807}`},
		{"out_of_range", `{"activeUserCount":9223372036854775808}`},
		{"duplicate_key", `{"activeUserCount":1,"activeUserCount":2}`},
	}

	var res []map[string]any
	for _, d := range docs {
		row := map[string]any{"name": d.name, "in": d.doc}
		probe(row, func() {
			var out model.ServerLimits
			err := json.Unmarshal([]byte(d.doc), &out)
			row["ok"] = err == nil
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}
			row["active_user_count"] = out.ActiveUserCount
			row["max_users_limit"] = out.MaxUsersLimit
			row["json_after"] = mustMarshal(out)
		})
		res = append(res, row)
	}
	return res
}
