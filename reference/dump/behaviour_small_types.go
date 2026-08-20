package main

// Behavioural oracle for six small model files ported together, written to
// fixtures/behaviour_small_types.json:
//
//	channel_stats.go   team_search.go   permalink.go
//	cluster_info.go    push_response.go read_receipt.go
//
// Five of the six are wire-only and their fixtures come from the reflective registry. What is
// here is the logic the registry cannot reach, and in each case it is something a reading gets
// wrong:
//
//  1. **`ChannelStats`'s three `_()` accessors return `float64`.** They exist for telemetry, and
//     the conversion is lossy above 2^53 — `float64(int64)` silently rounds. The corpus drives
//     the exact boundary so the Rust port has to reproduce the rounding rather than the integer.
//
//  2. **`TeamSearch.IsPaginated()` requires *both* pointers**, not either. A search with a page
//     and no per-page is not paginated, and the two nil-ness combinations are what separate `&&`
//     from `||`.
//
//  3. **`NewPreviewPost` guards `post` and then dereferences `team` and `channel` unguarded.**
//     A nil post returns nil; a nil team panics. That asymmetry is the whole function, and it is
//     recorded rather than tidied.
//
//  4. **`PushResponse`'s constants are the wire.** `PushStatusErrorMsg` is `"error"`, not
//     `"error_msg"` — a transcribed constant that drifts silently is exactly what
//     behaviour_version.go exists to prevent.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeSmallTypesBehaviourFixture(outDir string) error {
	out := map[string]any{
		"push_constants": map[string]any{
			"PushStatus":         model.PushStatus,
			"PushStatusOk":       model.PushStatusOk,
			"PushStatusFail":     model.PushStatusFail,
			"PushStatusRemove":   model.PushStatusRemove,
			"PushStatusErrorMsg": model.PushStatusErrorMsg,
		},
		"push_responses":   pushResponseAll(),
		"channel_stats":    channelStatsAll(),
		"team_search":      teamSearchAll(),
		"preview_post":     previewPostAll(),
		"zero_values":      smallTypeZeroValues(),
		"team_search_wire": teamSearchWireAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_small_types.json"), append(blob, '\n'), 0o644)
}

// The three constructors, marshalled. `PushResponse` is a `map[string]string`, so its wire form
// is the map itself — there is no struct and no tag to get wrong, only the keys.
func pushResponseAll() []map[string]any {
	return []map[string]any{
		{"name": "ok", "out": mustMarshal(model.NewOkPushResponse())},
		{"name": "remove", "out": mustMarshal(model.NewRemovePushResponse())},
		{"name": "error", "out": mustMarshal(model.NewErrorPushResponse("boom"))},
		// An empty message still writes the key: the constructor does not branch on it.
		{"name": "error_empty", "out": mustMarshal(model.NewErrorPushResponse(""))},
	}
}

// `float64(int64)` at and past the point where it stops being exact.
func channelStatsAll() []map[string]any {
	counts := []int64{
		0,
		1,
		-1,
		9007199254740992,  // 2^53, the last integer float64 represents exactly
		9007199254740993,  // 2^53+1, which float64 cannot represent — rounds down
		9007199254740995,  // rounds up
		-9007199254740993, // and the same on the negative side
		9223372036854775807,
	}

	var res []map[string]any
	for _, n := range counts {
		s := model.ChannelStats{ChannelId: "c1", MemberCount: n, GuestCount: n, PinnedPostCount: n, FilesCount: n}
		res = append(res, map[string]any{
			"in":                n,
			"member_count_":     s.MemberCount_(),
			"guest_count_":      s.GuestCount_(),
			"pinnedpost_count_": s.PinnedPostCount_(),
			"wire":              mustMarshal(&s),
		})
	}
	return res
}

// `Page != nil && PerPage != nil` — all four nil-ness combinations, plus zero values to show the
// check is on the pointer and not on what it points at.
func teamSearchAll() []map[string]any {
	zero, five := 0, 5
	cases := []struct {
		name          string
		page, perPage *int
	}{
		{"both_nil", nil, nil},
		{"page_only", &five, nil},
		{"per_page_only", nil, &five},
		{"both_set", &five, &five},
		{"both_zero", &zero, &zero},
		{"page_zero_per_page_nil", &zero, nil},
	}

	var res []map[string]any
	for _, c := range cases {
		t := model.TeamSearch{Term: "term", Page: c.page, PerPage: c.perPage}
		res = append(res, map[string]any{
			"name":         c.name,
			"is_paginated": t.IsPaginated(),
			"wire":         mustMarshal(&t),
		})
	}
	return res
}

// The nil guard that exists, and the two that do not.
func previewPostAll() []map[string]any {
	post := &model.Post{Id: "p1", Message: "hello"}
	team := &model.Team{Name: "core"}
	channel := &model.Channel{Id: "c1", DisplayName: "Town Square", Type: model.ChannelTypeOpen}

	cases := []struct {
		name    string
		post    *model.Post
		team    *model.Team
		channel *model.Channel
	}{
		{"all_present", post, team, channel},
		{"nil_post", nil, team, channel},
		{"nil_team", post, nil, channel},
		{"nil_channel", post, team, nil},
		{"all_nil", nil, nil, nil},
	}

	var res []map[string]any
	for _, c := range cases {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			pp := model.NewPreviewPost(c.post, c.team, c.channel)
			row["nil_result"] = pp == nil
			if pp != nil {
				row["out"] = mustMarshal(pp)
			}
			// And the enclosing wrapper, whose single field has no omitempty.
			row["permalink"] = mustMarshal(&model.Permalink{PreviewPost: pp})
		})
		res = append(res, row)
	}
	return res
}

// `omitempty` on eight pointers and `json:"-"` on four fields: the zero value is what proves
// which keys survive. `Permalink.PreviewPost` has no omitempty, so it serialises as null.
func smallTypeZeroValues() map[string]any {
	return map[string]any{
		"channel_stats": mustMarshal(&model.ChannelStats{}),
		"cluster_info":  mustMarshal(&model.ClusterInfo{}),
		"read_receipt":  mustMarshal(&model.ReadReceipt{}),
		"team_search":   mustMarshal(&model.TeamSearch{}),
		"permalink":     mustMarshal(&model.Permalink{}),
		"preview_post":  mustMarshal(&model.PreviewPost{}),
		"push_response": mustMarshal(model.PushResponse{}),
	}
}

// Decode probes: the four `json:"-"` fields must be unreachable from a request body, which is a
// security property on `IncludePolicyEnforced` ("never decoded from a request", team_search.go)
// and not merely a tag.
func teamSearchWireAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"empty_object", `{}`},
		{"term_only", `{"term":"eng"}`},
		{"pagination", `{"term":"eng","page":2,"per_page":50}`},
		{"page_zero", `{"page":0,"per_page":0}`},
		{"bools", `{"allow_open_invite":true,"group_constrained":false,"include_group_constrained":true,"exclude_policy_constrained":false}`},
		{"policy_id", `{"policy_id":"pol1"}`},
		{"policy_id_null", `{"policy_id":null}`},
		// The four dashed fields, offered under their Go field names and their snake_case guesses.
		{"dashed_fields", `{"IncludePolicyID":true,"include_policy_id":true,"IncludeDeleted":true,` +
			`"include_deleted":true,"TeamType":"O","team_type":"O","IncludePolicyEnforced":true,` +
			`"include_policy_enforced":true}`},
		{"unknown_key", `{"nope":1,"term":"eng"}`},
	}

	var res []map[string]any
	for _, c := range docs {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			var t model.TeamSearch
			if err := json.Unmarshal([]byte(c.doc), &t); err != nil {
				row["err"] = err.Error()
				return
			}
			row["err"] = nil
			row["out"] = mustMarshal(&t)
			row["is_paginated"] = t.IsPaginated()
			row["include_policy_id_nil"] = t.IncludePolicyID == nil
			row["include_deleted_nil"] = t.IncludeDeleted == nil
			row["team_type_nil"] = t.TeamType == nil
			row["include_policy_enforced_nil"] = t.IncludePolicyEnforced == nil
		})
		res = append(res, row)
	}
	return res
}
