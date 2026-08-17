package main

// Behavioural oracle for model/file_info_search_results.go, written to
// fixtures/behaviour_file_info_search_results.json.
//
// Eighteen lines: a `map[string][]string` alias, a struct, and a constructor that assigns two
// fields. There is no `ToJSON`, no `EncodeJSON`, no `ForPlugin` and no `Auditable` — so unlike
// `post_search_results.go`, which it otherwise duplicates, *the type declaration is the whole
// file*:
//
//	type FileInfoSearchResults struct {
//	    *FileInfoList
//	    Matches FileInfoSearchMatches `json:"matches"`
//	}
//
// Two consequences, and both are wire surface:
//
//  1. **The embed flattens, and a nil embed drops five keys.** `order`, `file_infos` and the
//     three scalars sit beside `matches` in one flat object; there is no `file_info_list` key.
//     When the pointer is nil, `encoding/json` skips every field whose index path runs through
//     it, so the document is `{"matches":…}` alone rather than five nulls. `MakeFileInfoSearch
//     Results` takes the list from its caller, so nil is reachable by construction.
//
//  2. **Which keys allocate the embed is itself the wire format.** Go allocates the pointer
//     lazily, the first time a decode walks into it. Whether an unknown key does it, whether an
//     explicit `null` does it, and whether `matches` alone does it are three separate questions
//     with three separately measurable answers — the corpus asks all of them, because a serde
//     port cannot express "allocate iff a recognised key was present" with `flatten` and has to
//     be told the key set.
//
// The set is read off the struct tags with reflection rather than transcribed, so a field added
// upstream fails a Rust test instead of silently narrowing it. `FileInfoList` has no `json:"-"`
// field, which is the one place it is NOT the same shape as `PostList` — there,
// `burn_on_read_posts` is a promoted field that is nevertheless an unknown key.
//
// Determinism: fixed documents only. No `NewId`, no `time.Now` — see [D-032]. The `filInfo*`
// constants and `dumpFileInfoList` are shared with behaviour_file_info_list.go.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeFileInfoSearchResultsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"wire":                     fisrWireAll(),
		"make":                     fisrMakeAll(),
		"matches_wire":             fisrMatchesWireAll(),
		"file_info_list_wire_keys": fisrFileInfoListWireKeys(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_file_info_search_results.json"), append(blob, '\n'), 0o644)
}

// --- helpers ----------------------------------------------------------------------------------

func fisrFromJSON(blob string) *model.FileInfoSearchResults {
	var fisr model.FileInfoSearchResults
	if err := json.Unmarshal([]byte(blob), &fisr); err != nil {
		panic(err)
	}
	return &fisr
}

// dumpSearchedFileInfos records the whole value plus the two things the JSON cannot show: whether
// the embedded pointer is nil (a nil embed and an embed full of zero values differ by five keys)
// and whether Matches is nil rather than empty.
func dumpSearchedFileInfos(fisr *model.FileInfoSearchResults) map[string]any {
	if fisr == nil {
		return map[string]any{"nil": true}
	}
	row := map[string]any{
		"json":        mustMarshal(fisr),
		"list_nil":    fisr.FileInfoList == nil,
		"matches_nil": fisr.Matches == nil,
	}
	if fisr.FileInfoList != nil {
		row["list"] = dumpFileInfoList(fisr.FileInfoList)
	}
	return row
}

// fisrCorpus is the shared input set. The first eleven exist to pin the lazy allocation of the
// embedded pointer; the rest carry real lists so the flattened key order can be asserted.
var fisrCorpus = []struct{ name, doc string }{
	{"zero", `{}`},
	{"matches_only", `{"matches":{"f1":["alpha","beta"]}}`},
	{"matches_null", `{"matches":null}`},
	{"matches_empty", `{"matches":{}}`},
	// A nil slice as a map value: Go keeps the key and re-emits it as null.
	{"matches_nil_slice", `{"matches":{"f1":null,"f2":["x"]}}`},
	{"matches_empty_slice", `{"matches":{"f1":[]}}`},
	{"matches_unsorted_keys", `{"matches":{"z":["1"],"a":["2"],"m":[]}}`},
	// One promoted key, explicitly null — enough to allocate the embed?
	{"list_null_only", `{"order":null}`},
	{"list_scalar_only", `{"next_file_info_id":"n1"}`},
	{"list_zero_scalar_only", `{"first_inaccessible_file_time":0}`},
	// Not a promoted key. FileInfoList has no json:"-" field, so this is the only unknown shape
	// — the case post_search_results.go also gets from `burn_on_read_posts`.
	{"unknown_key_only", `{"nope":1}`},
	// Go matches field names case-insensitively; we do not ([D-040]).
	{"uppercase_key_only", `{"ORDER":[]}`},
	{"list_without_matches", `{"order":["f1"],"file_infos":{"f1":` + filInfo1 + `}}`},
	{"full", `{"order":["f2","f1"],"file_infos":{"f1":` + filInfo1 + `,"f2":` + filInfo2 + `},` +
		`"next_file_info_id":"n1","prev_file_info_id":"v1",` +
		`"first_inaccessible_file_time":1700000000000,` +
		`"matches":{"f1":["one"],"f2":["two","three"]}}`},
	{"order_without_info", `{"order":["f1","missing"],"file_infos":{"f1":` + filInfo1 + `},"matches":{}}`},
	{"empty_collections", `{"order":[],"file_infos":{},"matches":{}}`},
	// Go's map[string]*FileInfo accepts a nil value and ours cannot ([D-033]).
	{"nil_info_in_map", `{"order":["f1"],"file_infos":{"f1":null},"matches":{"f1":["x"]}}`},
}

// --- the wire format --------------------------------------------------------------------------

// fisrWireAll drives both consequences: which documents allocate the embed, and what a nil embed
// marshals to. Every row is recorded byte-exact, so the flattened key order — promoted fields
// first, `matches` last — is asserted rather than assumed.
func fisrWireAll() []map[string]any {
	var res []map[string]any
	for _, c := range fisrCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			row["out"] = dumpSearchedFileInfos(fisrFromJSON(c.doc))
		})
		res = append(res, row)
	}
	return res
}

// --- MakeFileInfoSearchResults ------------------------------------------------------------------

// fisrMakeAll pins the constructor, which does nothing but assign — including assigning a nil
// list, the state that drops five keys from the wire. Note it uses positional initialisation
// (`&FileInfoSearchResults{fileInfos, matches}`), so a field added upstream would fail to compile
// there rather than silently going unset.
func fisrMakeAll() []map[string]any {
	var res []map[string]any

	add := func(name string, fil *model.FileInfoList, m model.FileInfoSearchMatches) {
		row := map[string]any{"name": name}
		probe(row, func() {
			row["out"] = dumpSearchedFileInfos(model.MakeFileInfoSearchResults(fil, m))
		})
		res = append(res, row)
	}

	add("both_nil", nil, nil)
	add("nil_list_with_matches", nil, model.FileInfoSearchMatches{"f1": {"alpha"}})
	add("new_list_nil_matches", model.NewFileInfoList(), nil)
	add("zero_list_nil_matches", &model.FileInfoList{}, nil)
	add("new_list_empty_matches", model.NewFileInfoList(), model.FileInfoSearchMatches{})
	add("decoded_list", filFromJSON(`{"order":["f1"],"file_infos":{"f1":`+filInfo1+`}}`),
		model.FileInfoSearchMatches{"f1": {"one"}})
	add("nil_slice_value", model.NewFileInfoList(), model.FileInfoSearchMatches{"f1": nil})

	// The zero value of the type itself, which is what a caller gets from
	// `var fisr FileInfoSearchResults` — a nil embed with nil matches.
	row := map[string]any{"name": "zero_value"}
	probe(row, func() {
		row["out"] = dumpSearchedFileInfos(&model.FileInfoSearchResults{})
	})
	res = append(res, row)

	return res
}

// --- FileInfoSearchMatches --------------------------------------------------------------------

// fisrMatchesWireAll pins the alias on its own, away from the embed. Two things are being
// measured: Go sorts map keys when marshalling, and a nil `[]string` value survives the round
// trip as `null` rather than collapsing to `[]`.
func fisrMatchesWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.FileInfoSearchMatches
	}{
		{"nil", nil},
		{"empty", model.FileInfoSearchMatches{}},
		{"one", model.FileInfoSearchMatches{"f1": {"alpha"}}},
		{"nil_value", model.FileInfoSearchMatches{"f1": nil}},
		{"empty_value", model.FileInfoSearchMatches{"f1": {}}},
		{"unsorted", model.FileInfoSearchMatches{"z": {"1"}, "a": {"2"}, "m": {}}},
		{"empty_key", model.FileInfoSearchMatches{"": {"x"}}},
		{"escaped", model.FileInfoSearchMatches{"<a>&": {"<b>", " "}}},
		{"duplicate_values", model.FileInfoSearchMatches{"f1": {"x", "x"}}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			row["out"] = mustMarshal(c.in)
			row["nil"] = c.in == nil
		})
		res = append(res, row)
	}
	return res
}

// --- the promoted key set -----------------------------------------------------------------------

// fisrFileInfoListWireKeys is the list of JSON keys the embedded *FileInfoList contributes, read
// off the struct tags rather than transcribed. The Rust port needs exactly this set to decide
// whether to allocate its Option<FileInfoList>; a field added upstream would otherwise silently
// stop allocating for documents that carry only that field.
func fisrFileInfoListWireKeys() []string {
	return expectedKeys(reflect.TypeOf(model.FileInfoList{}))
}
