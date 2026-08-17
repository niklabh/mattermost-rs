package main

// Behavioural oracle for model/file_info_list.go, written to
// fixtures/behaviour_file_info_list.json.
//
// `FileInfoList` is `PostList`'s twin and the temptation is to port it by copying post_list.rs
// and renaming. Five things differ, and every one of them is a place that copy would be wrong:
//
//  1. **`ToSlice` never pre-allocates**, so an empty `Order` returns a **nil** slice where
//     `PostList.ToSlice` returns an empty allocated one whenever `Posts` is non-empty. No Go
//     call site can see the difference, but the flag is recorded either way.
//
//  2. **`MakeNonNil` does not recurse.** `PostList.MakeNonNil` walks into every post and calls
//     `post.MakeNonNil()`; this one materialises the two collections and stops.
//
//  3. **`AddFileInfo` nil-checks its map and then dereferences its argument.** `PostList.AddPost`
//     is the other way round: it checks nothing and crashes on the `BurnOnReadPosts` write
//     ([D-052]). Here the crash needs a nil `*FileInfo`, which `Extend` can produce from a map
//     holding a nil value. Both probed under `recover`.
//
//  4. **`Etag` is character-for-character the same function as `PostList.Etag`** — the same
//     maximum over `(UpdateAt, Id)` seeded with `(0, "0")`, and the same `Order[0]` prefix. Worth
//     recording rather than assuming a difference: the port should reuse post_list.rs's reasoning
//     verbatim. The `etag_reversed` column measures the `Order[0]` dependence, which applies to
//     both types and which post_list.rs's module docs used to understate.
//
//  5. **There is no `Clone`, no `ForPlugin`, no `StripActionIntegrations` and no `ToJSON`.** The
//     type is a plain container; nothing here copies or sanitises.
//
// Everything else is shared with post_list.go and re-measured rather than assumed: the
// nil-against-empty table per method, `sort.Slice`'s instability ([D-051]) and the dereference
// of an order id with no matching file.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeFileInfoListBehaviourFixture(outDir string) error {
	out := map[string]any{
		"new":               fileInfoListNew(),
		"wire":              fileInfoListWireAll(),
		"to_slice":          fileInfoListToSliceAll(),
		"make_non_nil":      fileInfoListMakeNonNilAll(),
		"add_order":         fileInfoListAddOrderAll(),
		"add_file_info":     fileInfoListAddFileInfoAll(),
		"unique_order":      fileInfoListUniqueOrderAll(),
		"extend":            fileInfoListExtendAll(),
		"sort_by_create_at": fileInfoListSortAll(),
		"sort_ties":         fileInfoListSortTies(),
		"etag":              fileInfoListEtagAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_file_info_list.json"), append(blob, '\n'), 0o644)
}

// --- helpers ----------------------------------------------------------------------------------

func filFromJSON(blob string) *model.FileInfoList {
	var fil model.FileInfoList
	if err := json.Unmarshal([]byte(blob), &fil); err != nil {
		panic(err)
	}
	return &fil
}

// dumpFileInfoList records the whole list plus the nil-ness of both collections, which the JSON
// cannot show: `order` has no omitempty, so nil and empty both appear — as `null` and `[]`.
func dumpFileInfoList(fil *model.FileInfoList) map[string]any {
	if fil == nil {
		return map[string]any{"nil": true}
	}
	return map[string]any{
		"json":      mustMarshal(fil),
		"order_nil": fil.Order == nil,
		"infos_nil": fil.FileInfos == nil,
	}
}

const (
	filInfo1 = `{"id":"f1","user_id":"u1","channel_id":"c1","create_at":100,"update_at":100,` +
		`"name":"one.txt","extension":"txt","size":11,"mime_type":"text/plain"}`
	filInfo2 = `{"id":"f2","user_id":"u2","channel_id":"c1","create_at":200,"update_at":300,` +
		`"name":"two.png","extension":"png","size":22,"mime_type":"image/png"}`
	filInfo3 = `{"id":"f3","user_id":"u1","channel_id":"c2","create_at":50,"update_at":50,` +
		`"name":"three.pdf","extension":"pdf","size":33,"mime_type":"application/pdf"}`
	// update_at 0 with an id above "0": the Etag seed is (0, "0"), and this beats it.
	filInfoZero = `{"id":"zz","user_id":"u1","channel_id":"c1","create_at":0,"update_at":0,` +
		`"name":"zero.txt","extension":"txt","size":0,"mime_type":"text/plain"}`
	// ...and an id below "0", which does not.
	filInfoBelow = `{"id":"!!","user_id":"u1","channel_id":"c1","create_at":0,"update_at":0,` +
		`"name":"below.txt","extension":"txt","size":0,"mime_type":"text/plain"}`
)

// fileInfoListCorpus is the shared input set; every section runs all of it.
var fileInfoListCorpus = []struct{ name, doc string }{
	{"zero", `{}`},
	{"explicit_nulls", `{"order":null,"file_infos":null}`},
	{"empty", `{"order":[],"file_infos":{}}`},
	{"one", `{"order":["f1"],"file_infos":{"f1":` + filInfo1 + `}}`},
	{"two", `{"order":["f2","f1"],"file_infos":{"f1":` + filInfo1 + `,"f2":` + filInfo2 + `}}`},
	{"three", `{"order":["f1","f2","f3"],"file_infos":{"f1":` + filInfo1 + `,"f2":` + filInfo2 + `,"f3":` + filInfo3 + `}}`},
	// An order id with no file: ToSlice yields a nil element and SortByCreateAt dereferences it.
	{"order_without_info", `{"order":["f1","missing"],"file_infos":{"f1":` + filInfo1 + `}}`},
	// A file with no order entry: on the wire, invisible to ToSlice, and still counted by Etag.
	{"info_without_order", `{"order":[],"file_infos":{"f1":` + filInfo1 + `}}`},
	{"duplicate_order", `{"order":["f1","f1","f2","f1"],"file_infos":{"f1":` + filInfo1 + `,"f2":` + filInfo2 + `}}`},
	{"scalars", `{"order":[],"file_infos":{},"next_file_info_id":"n1","prev_file_info_id":"v1",` +
		`"first_inaccessible_file_time":1700000000000}`},
	{"etag_seed_beaten", `{"order":["zz"],"file_infos":{"zz":` + filInfoZero + `}}`},
	{"etag_seed_holds", `{"order":["!!"],"file_infos":{"!!":` + filInfoBelow + `}}`},
	{"etag_tie", `{"order":["f1"],"file_infos":{"f1":` + filInfo1 + `,"aa":` + strings.Replace(filInfo1, `"id":"f1"`, `"id":"aa"`, 1) + `}}`},
	// Go's map[string]*FileInfo accepts a nil value; our BTreeMap<String, FileInfo> cannot
	// ([D-033]). Reachable here through Extend, which passes the value to AddFileInfo.
	{"nil_info_in_map", `{"order":["f1"],"file_infos":{"f1":null}}`},
}

// --- NewFileInfoList ----------------------------------------------------------------------------

// fileInfoListNew pins that the constructor materialises both collections, so a freshly built
// list serialises with `[]`/`{}` where a decoded one carries `null`.
func fileInfoListNew() map[string]any {
	return dumpFileInfoList(model.NewFileInfoList())
}

// --- the wire format ------------------------------------------------------------------------------

func fileInfoListWireAll() []map[string]any {
	var res []map[string]any
	for _, c := range fileInfoListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			row["out"] = dumpFileInfoList(filFromJSON(c.doc))
		})
		res = append(res, row)
	}
	return res
}

// --- ToSlice --------------------------------------------------------------------------------------

// fileInfoListToSliceAll records `nil_slice` because this ToSlice never pre-allocates — unlike
// PostList's, which does whenever the map is non-empty.
func fileInfoListToSliceAll() []map[string]any {
	var res []map[string]any
	for _, c := range fileInfoListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			s := filFromJSON(c.doc).ToSlice()
			row["count"] = len(s)
			row["nil_slice"] = s == nil
			row["out"] = mustMarshal(s)
		})
		res = append(res, row)
	}
	return res
}

// --- MakeNonNil -----------------------------------------------------------------------------------

// fileInfoListMakeNonNilAll also records each file's own JSON, so a future reader can see that
// this MakeNonNil does *not* recurse into them the way PostList's does.
func fileInfoListMakeNonNilAll() []map[string]any {
	var res []map[string]any
	for _, c := range fileInfoListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			fil := filFromJSON(c.doc)
			fil.MakeNonNil()
			row["out"] = dumpFileInfoList(fil)
		})
		res = append(res, row)
	}
	return res
}

// --- AddOrder -------------------------------------------------------------------------------------

func fileInfoListAddOrderAll() []map[string]any {
	var res []map[string]any
	for _, c := range fileInfoListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			fil := filFromJSON(c.doc)
			fil.AddOrder("added")
			// Twice, because the capacity hint only applies to the first materialisation.
			fil.AddOrder("added")
			row["out"] = dumpFileInfoList(fil)
		})
		res = append(res, row)
	}
	return res
}

// --- AddFileInfo ----------------------------------------------------------------------------------

// fileInfoListAddFileInfoAll covers the two arguments that matter: a real file, and nil — which
// AddFileInfo dereferences for its Id. The nil case is reachable through Extend.
func fileInfoListAddFileInfoAll() []map[string]any {
	var res []map[string]any
	for _, c := range fileInfoListCorpus {
		for _, arg := range []string{"real", "nil", "empty_id"} {
			row := map[string]any{"name": c.name + "/" + arg, "in": c.doc, "arg": arg}
			probe(row, func() {
				fil := filFromJSON(c.doc)
				switch arg {
				case "real":
					var info model.FileInfo
					if err := json.Unmarshal([]byte(filInfo3), &info); err != nil {
						panic(err)
					}
					fil.AddFileInfo(&info)
				case "empty_id":
					fil.AddFileInfo(&model.FileInfo{})
				case "nil":
					fil.AddFileInfo(nil)
				}
				row["out"] = dumpFileInfoList(fil)
			})
			res = append(res, row)
		}
	}
	return res
}

// --- UniqueOrder ----------------------------------------------------------------------------------

func fileInfoListUniqueOrderAll() []map[string]any {
	var res []map[string]any
	for _, c := range fileInfoListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			fil := filFromJSON(c.doc)
			fil.UniqueOrder()
			row["out"] = dumpFileInfoList(fil)
		})
		res = append(res, row)
	}
	return res
}

// --- Extend ---------------------------------------------------------------------------------------

// fileInfoListExtendAll crosses the corpus with itself. Extend ranges over the *other* list's map,
// whose iteration order Go randomises — the result is order-independent only because the writes
// are keyed, which is worth proving rather than assuming, so each pair is run twice and the two
// answers compared.
func fileInfoListExtendAll() []map[string]any {
	var res []map[string]any
	for _, a := range fileInfoListCorpus {
		for _, b := range fileInfoListCorpus {
			row := map[string]any{"name": a.name + "+" + b.name, "in": a.doc, "other": b.doc}
			probe(row, func() {
				first := filFromJSON(a.doc)
				first.Extend(filFromJSON(b.doc))
				row["out"] = dumpFileInfoList(first)

				second := filFromJSON(a.doc)
				second.Extend(filFromJSON(b.doc))
				row["deterministic"] = mustMarshal(first) == mustMarshal(second)
			})
			res = append(res, row)
		}
	}
	return res
}

// --- SortByCreateAt -------------------------------------------------------------------------------

func fileInfoListSortAll() []map[string]any {
	var res []map[string]any
	for _, c := range fileInfoListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			fil := filFromJSON(c.doc)
			fil.SortByCreateAt()
			row["out"] = dumpFileInfoList(fil)
		})
		res = append(res, row)
	}
	return res
}

// fileInfoListSortTies rebuilds [D-051]'s tie corpus for this type. sort.Slice is documented as
// unstable, `Order` is on the wire, and the divergence needs both a long list and interleaved
// keys — so the sizes below straddle the twelve-element threshold where Go stops running
// insertion sort.
func fileInfoListSortTies() []map[string]any {
	build := func(n int, createAt func(i int) int64) *model.FileInfoList {
		fil := model.NewFileInfoList()
		for i := range n {
			id := "s" + strconv.Itoa(i)
			fil.AddOrder(id)
			fil.AddFileInfo(&model.FileInfo{Id: id, CreateAt: createAt(i)})
		}
		return fil
	}

	cases := []struct {
		name     string
		n        int
		createAt func(i int) int64
	}{
		{"all_tied_2", 2, func(int) int64 { return 5 }},
		{"all_tied_3", 3, func(int) int64 { return 5 }},
		{"all_tied_13", 13, func(int) int64 { return 5 }},
		{"all_tied_20", 20, func(int) int64 { return 5 }},
		{"two_groups_4", 4, func(i int) int64 { return int64(i % 2) }},
		{"two_groups_20", 20, func(i int) int64 { return int64(i % 2) }},
		{"distinct_20", 20, func(i int) int64 { return int64(i) }},
	}

	var res []map[string]any
	for _, c := range cases {
		row := map[string]any{"name": c.name, "n": c.n}
		probe(row, func() {
			fil := build(c.n, c.createAt)
			row["in_order"] = append([]string(nil), fil.Order...)
			fil.SortByCreateAt()
			row["out_order"] = append([]string(nil), fil.Order...)

			// What a *stable* sort would have produced, for the same input.
			stable := build(c.n, c.createAt)
			sort.SliceStable(stable.Order, func(i, j int) bool {
				return stable.FileInfos[stable.Order[i]].CreateAt > stable.FileInfos[stable.Order[j]].CreateAt
			})
			row["stable_order"] = append([]string(nil), stable.Order...)
			row["stable_agrees"] = fmt.Sprint(fil.Order) == fmt.Sprint(stable.Order)

			// Both are correct sorts, which is what bounds the damage: the create_at sequence
			// is identical either way.
			seq := make([]int64, 0, len(fil.Order))
			for _, id := range fil.Order {
				seq = append(seq, fil.FileInfos[id].CreateAt)
			}
			row["create_at_sequence"] = seq
		})
		res = append(res, row)
	}
	return res
}

// --- Etag -----------------------------------------------------------------------------------------

// fileInfoListEtagAll drives the difference from PostList.Etag: the first component is Order[0],
// so this etag is order-dependent while PostList's is not. The reordered variants prove it.
func fileInfoListEtagAll() []map[string]any {
	var res []map[string]any
	for _, c := range fileInfoListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			fil := filFromJSON(c.doc)
			row["etag"] = fil.Etag()

			// Reverse the order and recompute: a different answer means Order[0] is load-bearing.
			for i, j := 0, len(fil.Order)-1; i < j; i, j = i+1, j-1 {
				fil.Order[i], fil.Order[j] = fil.Order[j], fil.Order[i]
			}
			row["etag_reversed"] = fil.Etag()
		})
		res = append(res, row)
	}
	return res
}
