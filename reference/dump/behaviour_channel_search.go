package main

// Behavioural oracle for model/channel_search.go, written to
// fixtures/behaviour_channel_search.json.
//
// One constant and one struct with eighteen fields: thirteen bools, three strings, a `[]string`
// and two `*int`. No methods. It is the largest plain wire type left at this size, and almost all
// of it is uniform — so the corpus spends its rows on the three fields that are not.
//
// # `Page` and `PerPage` are `*int` WITH `omitempty`, and that is a three-way
//
//	Page    *int `json:"page,omitempty"`
//	PerPage *int `json:"per_page,omitempty"`
//
// Every other nillable field in the tree so far has been a pointer **without** `omitempty`
// (`ChannelMemberHistory.LeaveTime`, `ChannelData.Channel`), where nil is `null` and the key is
// always present. Here `omitempty` on a pointer tests **nil-ness, not the pointee**, so:
//
//	nil            -> the key is absent entirely
//	pointer to 0   -> "page": 0   (omitempty does NOT drop it — the pointer is non-nil)
//	pointer to 5   -> "page": 5
//
// Three states, three distinct documents. A port that modelled these as a plain `Option<i64>` with
// `skip_serializing_if = "Option::is_none"` gets all three right; one that used a bare `i64` with
// a zero-skip predicate would collapse the first two and drop a client's explicit `page=0`.
//
// # `TeamIds` has no `omitempty`
//
// So nil is `null` and empty is `[]`, and the key is always present — the opposite convention to
// the two pointers three lines below it, in the same struct.
//
// # The `int` question is settled, not re-measured
//
// `Page` is `*int`, the same platform-width type `ClusterStats` uses. [D-074] measured that
// against `int64` over eleven bounds on this host; this corpus cites it with a couple of bound
// probes rather than repeating the sweep.
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

func writeChannelSearchBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":    map[string]any{"ChannelSearchDefaultLimit": model.ChannelSearchDefaultLimit},
		"keys":         expectedKeys(reflect.TypeOf(model.ChannelSearch{})),
		"wire":         channelSearchWireAll(),
		"pointer_wire": channelSearchPointerWireAll(),
		"decode":       channelSearchDecodeAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_channel_search.json"), append(blob, '\n'), 0o644)
}

// --- the wire format ------------------------------------------------------------------------------

func channelSearchWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.ChannelSearch
	}{
		{"zero", model.ChannelSearch{}},
		{"term_only", model.ChannelSearch{Term: "town"}},
		// TeamIds has no omitempty, so nil and empty are two documents.
		{"team_ids_nil", model.ChannelSearch{Term: "t"}},
		{"team_ids_empty", model.ChannelSearch{Term: "t", TeamIds: []string{}}},
		{"team_ids_one", model.ChannelSearch{Term: "t", TeamIds: []string{"6bdz674pgq767e4jx75w4pf57a"}}},
		{"team_ids_several", model.ChannelSearch{
			TeamIds: []string{"6bdz674pgq767e4jx75w4pf57a", "qr6kf7ztp7yifxt4wm5xn51bke"},
		}},
		{"team_ids_empty_string", model.ChannelSearch{TeamIds: []string{""}}},
		// One bool set, to pin that the other twelve still emit `false`.
		{"one_bool", model.ChannelSearch{Public: true}},
		// Every bool set at once, which pins the emission order across the whole struct.
		{"all_bools", model.ChannelSearch{
			ExcludeDefaultChannels:             true,
			GroupConstrained:                   true,
			ExcludeGroupConstrained:            true,
			ExcludePolicyConstrained:           true,
			Public:                             true,
			Private:                            true,
			IncludeDeleted:                     true,
			IncludeSearchById:                  true,
			ExcludeRemote:                      true,
			Deleted:                            true,
			AccessControlPolicyEnforced:        true,
			ExcludeAccessControlPolicyEnforced: true,
		}},
		// The two near-duplicate pairs set together — nothing in the model package stops it, and
		// nothing in this file resolves the contradiction.
		{"contradictory_pairs", model.ChannelSearch{
			GroupConstrained:                   true,
			ExcludeGroupConstrained:            true,
			AccessControlPolicyEnforced:        true,
			ExcludeAccessControlPolicyEnforced: true,
			Public:                             true,
			Private:                            true,
		}},
		{"all_strings", model.ChannelSearch{
			Term:                        "town",
			NotAssociatedToGroup:        "g1",
			ParentAccessControlPolicyId: "p1",
		}},
		{"escaped_strings", model.ChannelSearch{Term: "<a>&b", NotAssociatedToGroup: "c d"}},
		// Everything at once.
		{"full", model.ChannelSearch{
			Term:                        "town",
			ExcludeDefaultChannels:      true,
			NotAssociatedToGroup:        "g1",
			TeamIds:                     []string{"6bdz674pgq767e4jx75w4pf57a"},
			Public:                      true,
			IncludeDeleted:              true,
			Page:                        model.NewPointer(2),
			PerPage:                     model.NewPointer(50),
			ParentAccessControlPolicyId: "p1",
		}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			row["json"] = mustMarshal(c.in)
			row["team_ids_nil"] = c.in.TeamIds == nil
			row["page_nil"] = c.in.Page == nil
		})
		res = append(res, row)
	}
	return res
}

// --- the omitempty pointers -------------------------------------------------------------------------

// channelSearchPointerWireAll is the file. `omitempty` on a pointer drops **nil**, not the zero
// pointee, so a pointer to 0 still emits `"page":0`. That is the distinction a port collapses if
// it models these as a bare int with a zero-skip predicate, and it is a real client state: page 0
// is the first page.
func channelSearchPointerWireAll() []map[string]any {
	corpus := []struct {
		name          string
		page, perPage *int
	}{
		{"both_nil", nil, nil},
		{"page_zero", model.NewPointer(0), nil},
		{"page_one", model.NewPointer(1), nil},
		{"per_page_zero", nil, model.NewPointer(0)},
		{"both_zero", model.NewPointer(0), model.NewPointer(0)},
		{"both_set", model.NewPointer(2), model.NewPointer(50)},
		{"negative", model.NewPointer(-1), model.NewPointer(-1)},
		// Cited from [D-074] rather than swept again: `int` holds the int64 range on this host.
		{"max_int", model.NewPointer(math.MaxInt), nil},
		{"min_int", model.NewPointer(math.MinInt), nil},
		{"default_limit", nil, model.NewPointer(model.ChannelSearchDefaultLimit)},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			in := model.ChannelSearch{Term: "t", Page: c.page, PerPage: c.perPage}
			blob := mustMarshal(in)
			row["json"] = blob

			// The three facts a port has to get right, named rather than left implicit in the
			// JSON: is the key there at all, is the pointer nil, and what is the pointee.
			var decoded map[string]any
			if err := json.Unmarshal([]byte(blob), &decoded); err != nil {
				panic(err)
			}
			_, pagePresent := decoded["page"]
			_, perPagePresent := decoded["per_page"]
			row["page_key_present"] = pagePresent
			row["per_page_key_present"] = perPagePresent
			row["page_nil"] = c.page == nil
			row["per_page_nil"] = c.perPage == nil
			if c.page != nil {
				row["page_value"] = int64(*c.page)
			}
			if c.perPage != nil {
				row["per_page_value"] = int64(*c.perPage)
			}
		})
		res = append(res, row)
	}
	return res
}

// --- the decode -------------------------------------------------------------------------------------

// channelSearchDecodeAll drives the inbound half, concentrating on the pointers: absent, null and
// a value are three distinct results and only the first two look alike.
func channelSearchDecodeAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"empty", `{}`},
		{"term_only", `{"term":"town"}`},
		// The three-way, from the wire side.
		{"page_absent", `{"term":"t"}`},
		{"page_null", `{"term":"t","page":null}`},
		{"page_zero", `{"term":"t","page":0}`},
		{"page_value", `{"term":"t","page":3}`},
		{"both_pages", `{"page":1,"per_page":25}`},
		// TeamIds: nil, empty and populated.
		{"team_ids_null", `{"team_ids":null}`},
		{"team_ids_empty", `{"team_ids":[]}`},
		{"team_ids_one", `{"team_ids":["t1"]}`},
		{"team_ids_null_element", `{"team_ids":[null]}`},
		// Bools.
		{"bool_true", `{"public":true}`},
		{"bool_null", `{"public":null}`},
		{"bool_string", `{"public":"true"}`},
		// Integer rules on the pointer.
		{"page_fractional", `{"page":1.5}`},
		{"page_quoted", `{"page":"1"}`},
		{"page_max_int64", `{"page":9223372036854775807}`},
		{"page_out_of_range", `{"page":9223372036854775808}`},
		{"unknown_key", `{"nope":1}`},
		// Go folds case against the tag ([D-040]); the tag here already has underscores.
		{"folded_key", `{"Exclude_Default_Channels":true}`},
	}

	var res []map[string]any
	for _, d := range docs {
		row := map[string]any{"name": d.name, "in": d.doc}
		probe(row, func() {
			var out model.ChannelSearch
			err := json.Unmarshal([]byte(d.doc), &out)
			row["ok"] = err == nil
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}
			row["page_nil"] = out.Page == nil
			if out.Page != nil {
				row["page_value"] = int64(*out.Page)
			}
			row["team_ids_nil"] = out.TeamIds == nil
			row["team_ids_len"] = len(out.TeamIds)
			row["public"] = out.Public
			row["exclude_default_channels"] = out.ExcludeDefaultChannels
			row["term"] = out.Term
			row["json_after"] = mustMarshal(out)
		})
		res = append(res, row)
	}
	return res
}
