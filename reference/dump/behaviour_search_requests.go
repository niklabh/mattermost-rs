package main

// Behavioural oracle for model/emoji_search.go and model/user_access_token_search.go, written to
// fixtures/behaviour_search_requests.json.
//
//	type EmojiSearch struct {
//	    Term       string `json:"term"`
//	    PrefixOnly bool   `json:"prefix_only"`
//	}
//
//	type UserAccessTokenSearch struct {
//	    Term string `json:"term"`
//	}
//
// **Three fields, and there is nothing here to measure.** Snake_case tags like most of the tree,
// no `omitempty`, no pointers, no methods, no constructors, no validation. This file exists to
// say that with evidence rather than by assertion: the key lists are read off the struct tags,
// each type is driven through its handful of reachable states, and the standing crate-wide
// divergences are confirmed to be the only ones.
//
// It is deliberately short. A corpus padded out to look proportionate to two ported types would
// be worse than a small one — it would imply these types have surface they do not have, and the
// next reader would go looking for it.
//
// The one thing worth pinning beyond the keys: `UserAccessTokenSearch` has a **single string
// field**, so its zero value is `{"term":""}` rather than `{}`. A port that reached for
// `omitempty` on a lone field — which looks harmless — would emit `{}` and a Go client comparing
// documents would see a different body.
//
// Determinism: fixed values only. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeSearchRequestsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"emoji_keys": expectedKeys(reflect.TypeOf(model.EmojiSearch{})),
		"token_keys": expectedKeys(reflect.TypeOf(model.UserAccessTokenSearch{})),
		"emoji_wire": searchRequestsEmojiWireAll(),
		"token_wire": searchRequestsTokenWireAll(),
		"decode":     searchRequestsDecodeAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_search_requests.json"), append(blob, '\n'), 0o644)
}

func searchRequestsEmojiWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.EmojiSearch
	}{
		{"zero", model.EmojiSearch{}},
		{"term_only", model.EmojiSearch{Term: "smile"}},
		{"prefix_only", model.EmojiSearch{PrefixOnly: true}},
		{"both", model.EmojiSearch{Term: "smile", PrefixOnly: true}},
		// Nothing validates the term, so anything a client sends round-trips.
		{"escaped", model.EmojiSearch{Term: "<a>&b"}},
		{"unicode", model.EmojiSearch{Term: "\U0001F600 日本"}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() { row["json"] = mustMarshal(c.in) })
		res = append(res, row)
	}
	return res
}

func searchRequestsTokenWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.UserAccessTokenSearch
	}{
		// The lone-field zero value: `{"term":""}`, not `{}`.
		{"zero", model.UserAccessTokenSearch{}},
		{"term", model.UserAccessTokenSearch{Term: "6bdz674pgq767e4jx75w4pf57a"}},
		{"escaped", model.UserAccessTokenSearch{Term: "<a>&b"}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() { row["json"] = mustMarshal(c.in) })
		res = append(res, row)
	}
	return res
}

// searchRequestsDecodeAll confirms the standing divergences are the only ones. Driven through
// EmojiSearch because it has both a string and a bool; UserAccessTokenSearch is a strict subset
// of that shape.
func searchRequestsDecodeAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"empty", `{}`},
		{"full", `{"term":"smile","prefix_only":true}`},
		{"partial", `{"term":"smile"}`},
		{"unknown_key", `{"nope":1}`},
		// The three standing crate-wide divergences.
		{"null_string", `{"term":null}`},
		{"null_bool", `{"prefix_only":null}`},
		{"folded_key", `{"Prefix_Only":true}`},
		{"duplicate_key", `{"term":"first","term":"second"}`},
		// Type mismatches, which both sides reject.
		{"number_into_string", `{"term":5}`},
		{"string_into_bool", `{"prefix_only":"true"}`},
	}

	var res []map[string]any
	for _, d := range docs {
		row := map[string]any{"name": d.name, "in": d.doc}
		probe(row, func() {
			var out model.EmojiSearch
			err := json.Unmarshal([]byte(d.doc), &out)
			row["ok"] = err == nil
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}
			row["term"] = out.Term
			row["prefix_only"] = out.PrefixOnly
			row["json_after"] = mustMarshal(out)
		})
		res = append(res, row)
	}
	return res
}
