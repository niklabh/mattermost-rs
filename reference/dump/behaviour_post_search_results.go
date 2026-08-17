package main

// Behavioural oracle for model/post_search_results.go, written to
// fixtures/behaviour_post_search_results.json.
//
// The file is 56 lines and three of its four methods are one-line wrappers over post_list.go.
// Everything interesting about it comes from the one line that is not a method:
//
//	type PostSearchResults struct {
//		*PostList
//		Matches PostSearchMatches `json:"matches"`
//	}
//
// Four consequences, none of which a reading of the source settles:
//
//  1. **The embed is a pointer, and it flattens.** `order`, `posts` and the scalars sit beside
//     `matches` in one flat object, with no nesting and no `post_list` key. That much is
//     documented Go behaviour; what is not is what happens when the pointer is **nil**, which
//     `MakePostSearchResults` accepts and `Auditable` explicitly guards against. `encoding/json`
//     walks an index path per field and a nil pointer anywhere along it skips the field — so a
//     nil embed should drop six keys rather than emit six nulls or crash. Measured, not assumed.
//
//  2. **Decoding decides the nil-ness of the embed from which keys are present.** Go allocates
//     the embedded pointer lazily, when it first walks into it for a matching key. So
//     `{"matches":{}}` should leave it nil while `{"order":null}` should allocate it — and an
//     *unknown* key must not. `burn_on_read_posts` is `json:"-"`, so it counts as unknown here.
//     The corpus drives all four shapes, because this is what a Rust `Option<PostList>` has to
//     reproduce and serde's `flatten` cannot express it.
//
//  3. **`ToJSON` mutates its receiver, where `(*PostList).ToJSON` does not.** Both open with a
//     shallow struct copy, but `PostList`'s copies the collections' owning struct by value, so
//     `StripActionIntegrations` swaps the map on the copy. Here the copy is of a struct holding
//     a **pointer**, so the strip lands on the shared `PostList` and the caller's integrations
//     are gone. Two lines that look identical, opposite side effects.
//
//  4. **`ForPlugin` shares `Matches` with its receiver** — `plCopy := *o` copies the map header
//     only. Probed by writing through the copy.
//
// `Auditable` is deferred with [D-028] and is not exercised here.

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writePostSearchResultsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"wire":                psrWireAll(),
		"make":                psrMakeAll(),
		"to_json":             psrToJSONAll(),
		"encode_json":         psrEncodeJSONAll(),
		"for_plugin":          psrForPluginAll(),
		"matches_wire":        psrMatchesWireAll(),
		"post_list_wire_keys": psrPostListWireKeys(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_post_search_results.json"), append(blob, '\n'), 0o644)
}

// --- helpers ----------------------------------------------------------------------------------

func psrFromJSON(blob string) *model.PostSearchResults {
	var psr model.PostSearchResults
	if err := json.Unmarshal([]byte(blob), &psr); err != nil {
		panic(err)
	}
	return &psr
}

// dumpSearchResults records the whole value plus the two things the JSON cannot show: whether the
// embedded pointer is nil (a nil embed and an embed full of zero values differ by six keys) and
// whether Matches is nil rather than empty.
func dumpSearchResults(psr *model.PostSearchResults) map[string]any {
	if psr == nil {
		return map[string]any{"nil": true}
	}
	row := map[string]any{
		"json":        mustMarshal(psr),
		"list_nil":    psr.PostList == nil,
		"matches_nil": psr.Matches == nil,
	}
	if psr.PostList != nil {
		row["list"] = dumpPostList(psr.PostList)
	}
	return row
}

// A post carrying metadata, which is the field ForPlugin strips.
const psrPostMeta = `{"id":"pm","create_at":7,"update_at":7,"user_id":"u1","channel_id":"c1",` +
	`"message":"meta","metadata":{"emojis":[{"id":"e1","name":"smile"}],"priority":{"priority":"urgent"}}}`

// psrCorpus is the shared input set. Every section below runs all of it, so a shape that matters
// for one method is measured for all of them. The first six exist to pin the lazy allocation of
// the embedded pointer described in the file comment.
var psrCorpus = []struct{ name, doc string }{
	{"zero", `{}`},
	{"matches_only", `{"matches":{"p1":["alpha","beta"]}}`},
	{"matches_null", `{"matches":null}`},
	{"matches_empty", `{"matches":{}}`},
	// A nil slice as a map value: Go keeps the key and re-emits it as null.
	{"matches_nil_slice", `{"matches":{"p1":null,"p2":["x"]}}`},
	{"matches_empty_slice", `{"matches":{"p1":[]}}`},
	{"matches_unsorted_keys", `{"matches":{"z":["1"],"a":["2"],"m":[]}}`},
	// One promoted key, explicitly null — enough to allocate the embed?
	{"list_null_only", `{"order":null}`},
	{"list_scalar_only", `{"next_post_id":"n1"}`},
	{"has_next_false", `{"has_next":false}`},
	// Neither of these is a promoted key: one is unknown, the other is json:"-".
	{"unknown_key_only", `{"nope":1}`},
	{"burn_key_only", `{"burn_on_read_posts":{}}`},
	// Go matches field names case-insensitively; we do not ([D-040]).
	{"uppercase_key_only", `{"ORDER":[]}`},
	{"list_without_matches", `{"order":["p1"],"posts":{"p1":` + plPost1 + `}}`},
	{"full", `{"order":["p2","p1"],"posts":{"p1":` + plPost1 + `,"p2":` + plPost2 + `},` +
		`"next_post_id":"n1","prev_post_id":"v1","has_next":true,` +
		`"first_inaccessible_post_time":1700000000000,` +
		`"matches":{"p1":["one"],"p2":["two","three"]}}`},
	{"with_attachments", `{"order":["pa"],"posts":{"pa":` + plPostAction + `},"matches":{"pa":["Click"]}}`},
	{"with_metadata", `{"order":["pm"],"posts":{"pm":` + psrPostMeta + `},"matches":{"pm":["meta"]}}`},
	{"order_without_post", `{"order":["p1","missing"],"posts":{"p1":` + plPost1 + `},"matches":{}}`},
	{"empty_collections", `{"order":[],"posts":{},"matches":{}}`},
}

// --- the wire format --------------------------------------------------------------------------

// psrWireAll drives the decode side of consequences 1 and 2: which documents allocate the embed,
// and what a nil embed marshals to.
func psrWireAll() []map[string]any {
	var res []map[string]any
	for _, c := range psrCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			row["out"] = dumpSearchResults(psrFromJSON(c.doc))
		})
		res = append(res, row)
	}
	return res
}

// --- MakePostSearchResults --------------------------------------------------------------------

// psrMakeAll pins the constructor, which does nothing but assign — including assigning a nil
// list, which is the state Auditable guards for and the one that drops six keys from the wire.
func psrMakeAll() []map[string]any {
	var res []map[string]any

	add := func(name string, pl *model.PostList, m model.PostSearchMatches) {
		row := map[string]any{"name": name}
		probe(row, func() {
			row["out"] = dumpSearchResults(model.MakePostSearchResults(pl, m))
		})
		res = append(res, row)
	}

	add("both_nil", nil, nil)
	add("nil_list_with_matches", nil, model.PostSearchMatches{"p1": {"alpha"}})
	add("new_list_nil_matches", model.NewPostList(), nil)
	add("zero_list_nil_matches", &model.PostList{}, nil)
	add("new_list_empty_matches", model.NewPostList(), model.PostSearchMatches{})
	add("decoded_list", plFromJSON(`{"order":["p1"],"posts":{"p1":`+plPost1+`}}`),
		model.PostSearchMatches{"p1": {"one"}})

	// The zero value of the type itself, which is what a caller gets from `var psr PostSearchResults`.
	row := map[string]any{"name": "zero_value"}
	probe(row, func() {
		row["out"] = dumpSearchResults(&model.PostSearchResults{})
	})
	res = append(res, row)

	return res
}

// --- ToJSON -----------------------------------------------------------------------------------

// psrToJSONAll measures consequence 3: `receiver_after` is the receiver re-marshalled once ToJSON
// has returned, so a shared strip shows up as an integration missing from a value the caller
// never handed over.
func psrToJSONAll() []map[string]any {
	var res []map[string]any
	for _, c := range psrCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			psr := psrFromJSON(c.doc)
			out, err := psr.ToJSON()
			row["out"] = out
			row["err"] = err != nil
			row["receiver_after"] = dumpSearchResults(psr)
		})
		res = append(res, row)
	}
	return res
}

// --- EncodeJSON -------------------------------------------------------------------------------

// psrEncodeJSONAll pins the trailing newline json.Encoder writes and json.Marshal does not, and
// records the receiver for the same reason ToJSON does.
func psrEncodeJSONAll() []map[string]any {
	var res []map[string]any
	for _, c := range psrCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			psr := psrFromJSON(c.doc)
			var buf bytes.Buffer
			err := psr.EncodeJSON(&buf)
			row["out"] = buf.String()
			row["err"] = err != nil
			row["receiver_after"] = dumpSearchResults(psr)
		})
		res = append(res, row)
	}
	return res
}

// --- ForPlugin --------------------------------------------------------------------------------

// psrForPluginAll measures consequence 4 alongside the metadata strip: `matches_aliased` is
// written through the *copy* and read back off the original.
func psrForPluginAll() []map[string]any {
	var res []map[string]any
	for _, c := range psrCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			psr := psrFromJSON(c.doc)
			cp := psr.ForPlugin()
			row["out"] = dumpSearchResults(cp)

			if cp.Matches != nil {
				cp.Matches["injected"] = []string{"x"}
				_, aliased := psr.Matches["injected"]
				row["matches_aliased"] = aliased
				delete(cp.Matches, "injected")
			} else {
				row["matches_aliased"] = nil
			}

			// The original must be untouched: ForPlugin replaces the pointer on the copy rather
			// than stripping through it, which is the opposite of what ToJSON does.
			row["original_after"] = dumpSearchResults(psr)
		})
		res = append(res, row)
	}
	return res
}

// --- PostSearchMatches ------------------------------------------------------------------------

// psrMatchesWireAll exercises the map type on its own. It is `map[string][]string`, so Go sorts
// the keys on the way out and a nil value survives the round trip as null — which is why the
// Rust side carries an Option per value rather than a bare Vec.
func psrMatchesWireAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"null", `null`},
		{"empty", `{}`},
		{"one", `{"p1":["a"]}`},
		{"nil_value", `{"p1":null}`},
		{"empty_value", `{"p1":[]}`},
		{"unsorted", `{"z":["1"],"A":["2"],"a":["3"],"m":null}`},
		{"empty_key", `{"":["a"]}`},
		{"duplicate_key", `{"p1":["a"],"p1":["b"]}`},
		{"escapes", `{"a<b>c&d":["x<y>z"]}`},
		{"unicode_keys", `{"é":["1"],"e":["2"],"éx":["3"]}`},
		{"many_values", `{"p1":["a","b","c",""]}`},
	}

	var res []map[string]any
	for _, c := range docs {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			var m model.PostSearchMatches
			if err := json.Unmarshal([]byte(c.doc), &m); err != nil {
				row["err"] = err.Error()
				return
			}
			row["err"] = nil
			row["out"] = mustMarshal(m)
			row["nil"] = m == nil
			row["len"] = len(m)
		})
		res = append(res, row)
	}
	return res
}

// --- the promoted key set ---------------------------------------------------------------------

// psrPostListWireKeys is the list of JSON keys the embedded *PostList contributes, read off the
// struct tags rather than transcribed. The Rust port needs exactly this set to decide whether to
// allocate its Option<PostList>, and a field added upstream would otherwise silently stop
// allocating.
func psrPostListWireKeys() []string {
	return expectedKeys(reflect.TypeOf(model.PostList{}))
}
