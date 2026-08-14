package main

// Behavioural oracle for model/post_list.go and model/wrangler.go, written to
// fixtures/behaviour_post_list.json.
//
// PostList is the return type of the first read endpoint that will be ported, so its wire
// format is load-bearing. Six things need Go's own answer rather than a reading of the source:
//
//  1. **Nil and empty are different, and half the methods silently convert one into the other.**
//     `Order` and `Posts` have no `omitempty`, so a nil map or slice reaches the client as
//     `null`. `Clone`, `StripActionIntegrations`, `UniqueOrder`, `MakeNonNil` and `ToJSON` each
//     materialise some subset of them into `[]`/`{}` — and no two do the same subset. Every
//     section below records nil-ness before and after so the subset is measured.
//
//  2. **`ToSlice` can return nil elements.** It walks `Order` and indexes `Posts`, so an order
//     id with no matching post yields a nil `*Post`. `AddOrder` takes an id with no post, so
//     this is reachable through the public API, not only through a malformed document.
//
//  3. **`BurnOnReadPosts` is `json:"-"` and therefore always nil on a decoded list** — while
//     `AddPost` writes to it unconditionally for a burn-on-read post. Assignment to a nil Go map
//     panics. Probed under `recover` rather than reasoned about.
//
//  4. **`Etag` is order-independent where `ChannelList.Etag` is not.** The tie-break on `v.Id`
//     turns the running maximum into a max over the pair `(UpdateAt, Id)` seeded with `(0, "0")`
//     — which matters because Go iterates a *map* here, so an order-dependent answer would be
//     nondeterministic. The seed is also reachable: a post with `update_at: 0` and an id greater
//     than `"0"` wins the tie against the seed.
//
//  5. **`sort.Slice` is not stable.** `SortByCreateAt` sorts `Order`, which is on the wire, so
//     the tie corpus below records Go's actual permutation at several list sizes.
//
//  6. **`BuildWranglerPostList` mutates its receiver** (`UniqueOrder` then `SortByCreateAt`)
//     before building anything, so the list a caller holds is reordered as a side effect. The
//     `list_after` column records that.

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

func writePostListBehaviourFixture(outDir string) error {
	out := map[string]any{
		"new":                       postListNew(),
		"wire":                      postListWireAll(),
		"clone":                     postListCloneAll(),
		"to_slice":                  postListToSliceAll(),
		"strip_action_integrations": postListStripAll(),
		"to_json":                   postListToJSONAll(),
		"encode_json":               postListEncodeJSONAll(),
		"make_non_nil":              postListMakeNonNilAll(),
		"add_order":                 postListAddOrderAll(),
		"add_post":                  postListAddPostAll(),
		"unique_order":              postListUniqueOrderAll(),
		"extend":                    postListExtendAll(),
		"sort_by_create_at":         postListSortAll(),
		"etag":                      postListEtagAll(),
		"is_channel_id":             postListIsChannelIDAll(),
		"build_wrangler_post_list":  postListWranglerAll(),
		"for_plugin":                postListForPluginAll(),
		"post_for_plugin":           postForPluginAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_post_list.json"), append(blob, '\n'), 0o644)
}

// --- helpers ----------------------------------------------------------------------------------

func plFromJSON(blob string) *model.PostList {
	var pl model.PostList
	if err := json.Unmarshal([]byte(blob), &pl); err != nil {
		panic(err)
	}
	return &pl
}

func mustMarshal(v any) string {
	b, err := json.Marshal(v)
	if err != nil {
		panic(err)
	}
	return string(b)
}

// dumpPostList records everything about a list that a JSON round trip alone would hide: the
// nil-ness of the three collections (a nil slice and an empty one both read as `[]` to someone
// skimming, and `BurnOnReadPosts` never appears in the JSON at all).
func dumpPostList(pl *model.PostList) map[string]any {
	if pl == nil {
		return map[string]any{"nil": true}
	}
	return map[string]any{
		"json":      mustMarshal(pl),
		"order_nil": pl.Order == nil,
		"posts_nil": pl.Posts == nil,
		"burn_nil":  pl.BurnOnReadPosts == nil,
		"burn":      mustMarshal(pl.BurnOnReadPosts),
	}
}

// probe runs f, recording whether it panicked. Several PostList methods dereference a nil *Post
// or assign to a nil map, and a crash is an answer worth pinning.
func probe(row map[string]any, f func()) {
	defer func() {
		if r := recover(); r != nil {
			row["panicked"] = true
		}
	}()
	row["panicked"] = false
	f()
}

const (
	plPost1 = `{"id":"p1","create_at":100,"update_at":100,"user_id":"u1","channel_id":"c1","message":"one"}`
	plPost2 = `{"id":"p2","create_at":200,"update_at":300,"user_id":"u2","channel_id":"c1","message":"two"}`
	plPost3 = `{"id":"p3","create_at":50,"update_at":50,"user_id":"u1","channel_id":"c2","message":"three"}`

	// An attachment carrying the two private fields StripActionIntegrations removes.
	plPostAction = `{"id":"pa","create_at":10,"update_at":10,"user_id":"u1","channel_id":"c1",` +
		`"props":{"attachments":[{"id":1,"actions":[{"id":"a1","name":"Click",` +
		`"integration":{"url":"https://example.com/hook","context":{"k":"v"}}}]}]}}`
)

// postListCorpus is the shared input set. Each document is fed to every section below, so a
// shape that matters for one method is measured for all of them.
var postListCorpus = []struct{ name, doc string }{
	{"zero", `{}`},
	{"explicit_nulls", `{"order":null,"posts":null}`},
	{"empty", `{"order":[],"posts":{}}`},
	{"one", `{"order":["p1"],"posts":{"p1":` + plPost1 + `}}`},
	{"two", `{"order":["p2","p1"],"posts":{"p1":` + plPost1 + `,"p2":` + plPost2 + `}}`},
	{"three", `{"order":["p1","p2","p3"],"posts":{"p1":` + plPost1 + `,"p2":` + plPost2 + `,"p3":` + plPost3 + `}}`},
	// An order id with no post: ToSlice yields a nil element and SortByCreateAt dereferences it.
	{"order_without_post", `{"order":["p1","missing"],"posts":{"p1":` + plPost1 + `}}`},
	// A post with no order entry: it is in Posts, on the wire, and invisible to ToSlice.
	{"post_without_order", `{"order":[],"posts":{"p1":` + plPost1 + `}}`},
	{"duplicate_order", `{"order":["p1","p1","p2","p1"],"posts":{"p1":` + plPost1 + `,"p2":` + plPost2 + `}}`},
	{"scalars", `{"order":[],"posts":{},"next_post_id":"n1","prev_post_id":"v1",` +
		`"has_next":true,"first_inaccessible_post_time":1700000000000}`},
	{"has_next_false", `{"has_next":false}`},
	{"with_attachments", `{"order":["pa"],"posts":{"pa":` + plPostAction + `}}`},
	{"burn_on_read", `{"order":["b1"],"posts":{"b1":{"id":"b1","type":"burn_on_read","create_at":5,"update_at":5,"channel_id":"c1","user_id":"u1"}}}`},
	// Go's map[string]*Post accepts a nil value; our BTreeMap<String, Post> cannot ([D-033]).
	{"nil_post_in_map", `{"order":["p1"],"posts":{"p1":null}}`},
}

// --- NewPostList ------------------------------------------------------------------------------

// postListNew pins that NewPostList materialises three of the collections and leaves HasNext nil,
// so a freshly built list serialises with `[]`/`{}` where a decoded one carries `null`.
func postListNew() map[string]any {
	return dumpPostList(model.NewPostList())
}

// --- the wire format --------------------------------------------------------------------------

type postListWireCase struct {
	Name string `json:"name"`
	In   string `json:"in"`
	Out  string `json:"out"`
	// Go's own round trip, recorded separately because `burn_on_read_posts` is json:"-" and
	// therefore lost by any decode.
	OrderNil bool `json:"order_nil"`
	PostsNil bool `json:"posts_nil"`
	BurnNil  bool `json:"burn_nil"`
}

func postListWireAll() []postListWireCase {
	res := make([]postListWireCase, 0, len(postListCorpus))
	for _, c := range postListCorpus {
		row := postListWireCase{Name: c.name, In: c.doc}
		func() {
			defer func() {
				if r := recover(); r != nil {
					row.Out = ""
				}
			}()
			pl := plFromJSON(c.doc)
			row.Out = mustMarshal(pl)
			row.OrderNil = pl.Order == nil
			row.PostsNil = pl.Posts == nil
			row.BurnNil = pl.BurnOnReadPosts == nil
		}()
		res = append(res, row)
	}
	return res
}

// --- Clone ------------------------------------------------------------------------------------

// postListCloneAll measures the two things Clone does that its name does not suggest: it
// materialises nil collections into empty ones, and it deep-copies the posts while leaving
// HasNext a shared pointer.
func postListCloneAll() []map[string]any {
	var res []map[string]any
	for _, c := range postListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			pl := plFromJSON(c.doc)
			// A burn-on-read post only reaches BurnOnReadPosts through AddPost, so seed it here
			// for the one case that has one, to prove Clone copies that map too.
			cl := pl.Clone()
			row["out"] = dumpPostList(cl)
			// Go aliases HasNext; mutating through the clone writes to the original.
			if cl.HasNext != nil {
				*cl.HasNext = !*cl.HasNext
				row["has_next_aliased"] = *pl.HasNext == *cl.HasNext
			} else {
				row["has_next_aliased"] = nil
			}
			// The posts themselves are deep copies: mutating one must not touch the other.
			for id := range cl.Posts {
				cl.Posts[id].Message = "mutated"
				row["posts_aliased"] = pl.Posts[id].Message == "mutated"
				break
			}
		})
		res = append(res, row)
	}
	return res
}

// --- ToSlice ----------------------------------------------------------------------------------

func postListToSliceAll() []map[string]any {
	var res []map[string]any
	for _, c := range postListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			pl := plFromJSON(c.doc)
			s := pl.ToSlice()
			row["count"] = len(s)
			row["nil_slice"] = s == nil
			row["out"] = mustMarshal(s)
		})
		res = append(res, row)
	}
	return res
}

// --- StripActionIntegrations --------------------------------------------------------------------

func postListStripAll() []map[string]any {
	var res []map[string]any
	for _, c := range postListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			pl := plFromJSON(c.doc)
			pl.StripActionIntegrations()
			row["out"] = dumpPostList(pl)
		})
		res = append(res, row)
	}
	return res
}

// --- ToJSON / EncodeJSON ------------------------------------------------------------------------

// postListToJSONAll records the receiver *after* the call as well as the output: ToJSON strips a
// copy, so `receiver_after` must still carry the integration. EncodeJSON strips the receiver, so
// its `receiver_after` must not.
func postListToJSONAll() []map[string]any {
	var res []map[string]any
	for _, c := range postListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			pl := plFromJSON(c.doc)
			s, err := pl.ToJSON()
			row["out"] = s
			row["err"] = err != nil
			row["receiver_after"] = dumpPostList(pl)
		})
		res = append(res, row)
	}
	return res
}

func postListEncodeJSONAll() []map[string]any {
	var res []map[string]any
	for _, c := range postListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			pl := plFromJSON(c.doc)
			var buf bytes.Buffer
			err := pl.EncodeJSON(&buf)
			row["out"] = buf.String()
			row["err"] = err != nil
			row["receiver_after"] = dumpPostList(pl)
		})
		res = append(res, row)
	}
	return res
}

// --- MakeNonNil -----------------------------------------------------------------------------

func postListMakeNonNilAll() []map[string]any {
	var res []map[string]any
	for _, c := range postListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			pl := plFromJSON(c.doc)
			pl.MakeNonNil()
			row["out"] = dumpPostList(pl)
		})
		res = append(res, row)
	}
	return res
}

// --- AddOrder / AddPost -------------------------------------------------------------------------

func postListAddOrderAll() []map[string]any {
	var res []map[string]any
	for _, c := range postListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc, "id": "added"}
		probe(row, func() {
			pl := plFromJSON(c.doc)
			pl.AddOrder("added")
			row["out"] = dumpPostList(pl)
		})
		res = append(res, row)
	}
	// Adding the same id twice does not deduplicate — that is UniqueOrder's job.
	row := map[string]any{"name": "twice", "in": `{}`, "id": "added"}
	probe(row, func() {
		pl := plFromJSON(`{}`)
		pl.AddOrder("added")
		pl.AddOrder("added")
		row["out"] = dumpPostList(pl)
	})
	return append(res, row)
}

// postListAddPostAll drives the nil-map panic: BurnOnReadPosts is json:"-", so it is nil on every
// decoded list, and AddPost assigns into it without a nil check for a burn-on-read post.
func postListAddPostAll() []map[string]any {
	posts := []struct{ name, doc string }{
		{"ordinary", plPost1},
		{"burn_on_read", `{"id":"b1","type":"burn_on_read","create_at":5,"update_at":5,"channel_id":"c1","user_id":"u1"}`},
		{"empty_id", `{"id":"","message":"no id"}`},
	}
	lists := []struct{ name, doc string }{
		{"zero", `{}`},
		{"empty", `{"order":[],"posts":{}}`},
		{"one", `{"order":["p1"],"posts":{"p1":` + plPost1 + `}}`},
	}

	var res []map[string]any
	for _, l := range lists {
		for _, p := range posts {
			row := map[string]any{"name": l.name + "_" + p.name, "in": l.doc, "post": p.doc}
			probe(row, func() {
				pl := plFromJSON(l.doc)
				pl.AddPost(postFromJSON(p.doc))
				row["out"] = dumpPostList(pl)
			})
			res = append(res, row)
		}
	}

	// The same three against a list built by NewPostList, where BurnOnReadPosts is not nil.
	for _, p := range posts {
		row := map[string]any{"name": "new_" + p.name, "in": "NewPostList()", "post": p.doc}
		probe(row, func() {
			pl := model.NewPostList()
			pl.AddPost(postFromJSON(p.doc))
			row["out"] = dumpPostList(pl)
		})
		res = append(res, row)
	}
	return res
}

// --- UniqueOrder / Extend -----------------------------------------------------------------------

func postListUniqueOrderAll() []map[string]any {
	var res []map[string]any
	for _, c := range postListCorpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			pl := plFromJSON(c.doc)
			pl.UniqueOrder()
			row["out"] = dumpPostList(pl)
		})
		res = append(res, row)
	}
	return res
}

func postListExtendAll() []map[string]any {
	pairs := []struct{ name, a, b string }{
		{"empty_into_empty", `{}`, `{}`},
		{"into_zero", `{}`, `{"order":["p1"],"posts":{"p1":` + plPost1 + `}}`},
		{"zero_other", `{"order":["p1"],"posts":{"p1":` + plPost1 + `}}`, `{}`},
		{"disjoint", `{"order":["p1"],"posts":{"p1":` + plPost1 + `}}`,
			`{"order":["p2"],"posts":{"p2":` + plPost2 + `}}`},
		// The overlapping id: other's post wins the map, and UniqueOrder keeps the first
		// occurrence of the id in Order.
		{"overlapping", `{"order":["p1"],"posts":{"p1":` + plPost1 + `}}`,
			`{"order":["p1","p2"],"posts":{"p1":{"id":"p1","message":"replaced","create_at":100,"update_at":100,"channel_id":"c1","user_id":"u1"},"p2":` + plPost2 + `}}`},
		{"other_has_order_only", `{"order":["p1"],"posts":{"p1":` + plPost1 + `}}`, `{"order":["ghost"],"posts":{}}`},
		{"other_burn_on_read", `{}`,
			`{"order":["b1"],"posts":{"b1":{"id":"b1","type":"burn_on_read","create_at":5,"update_at":5,"channel_id":"c1","user_id":"u1"}}}`},
	}

	var res []map[string]any
	for _, p := range pairs {
		row := map[string]any{"name": p.name, "a": p.a, "b": p.b}
		probe(row, func() {
			a := plFromJSON(p.a)
			b := plFromJSON(p.b)
			a.Extend(b)
			row["out"] = dumpPostList(a)
		})
		res = append(res, row)
	}
	return res
}

// --- SortByCreateAt ---------------------------------------------------------------------------

// postListSortAll pins the permutation, not just the multiset. sort.Slice is not stable, so the
// tie cases below are the only evidence for what Go actually emits at each list size — and Order
// goes on the wire.
// tiedList builds a list of n posts that all share one create_at, ordered "s00".."s(n-1)", so the
// output permutation is entirely the sort's doing.
func tiedList(n int) string {
	order := make([]string, 0, n)
	posts := make([]string, 0, n)
	for i := range n {
		id := "s" + strconv.Itoa(i)
		order = append(order, id)
		posts = append(posts, `"`+id+`":{"id":"`+id+`","create_at":100,"update_at":100,"channel_id":"c1","user_id":"u1"}`)
	}
	return `{"order":` + mustMarshal(order) + `,"posts":{` + strings.Join(posts, ",") + `}}`
}

// interleavedList alternates two create_at values across n posts, so every element has a tie
// partner and no prefix of the input is already sorted.
func interleavedList(n int) string {
	order := make([]string, 0, n)
	posts := make([]string, 0, n)
	for i := range n {
		id := "s" + strconv.Itoa(i)
		createAt := 100
		if i%2 == 1 {
			createAt = 200
		}
		order = append(order, id)
		posts = append(posts, `"`+id+`":{"id":"`+id+`","create_at":`+strconv.Itoa(createAt)+
			`,"update_at":100,"channel_id":"c1","user_id":"u1"}`)
	}
	return `{"order":` + mustMarshal(order) + `,"posts":{` + strings.Join(posts, ",") + `}}`
}

func postListSortAll() []map[string]any {
	sortPost := func(id string, createAt int64) string {
		return `"` + id + `":{"id":"` + id + `","create_at":` + strconv.FormatInt(createAt, 10) +
			`,"update_at":` + strconv.FormatInt(createAt, 10) + `,"channel_id":"c1","user_id":"u1"}`
	}
	list := func(order []string, posts ...string) string {
		return `{"order":` + mustMarshal(order) + `,"posts":{` + strings.Join(posts, ",") + `}}`
	}

	cases := []struct{ name, doc string }{
		{"empty", `{"order":[],"posts":{}}`},
		{"nulls", `{"order":null,"posts":null}`},
		{"one", list([]string{"a"}, sortPost("a", 100))},
		{"already_descending", list([]string{"a", "b"}, sortPost("a", 200), sortPost("b", 100))},
		{"ascending", list([]string{"a", "b"}, sortPost("a", 100), sortPost("b", 200))},
		{"three", list([]string{"a", "b", "c"}, sortPost("a", 100), sortPost("b", 300), sortPost("c", 200))},
		// Ties at four sizes: sort.Slice is unstable, so these record the permutation Go picks.
		{"tie_two", list([]string{"a", "b"}, sortPost("a", 100), sortPost("b", 100))},
		{"tie_three", list([]string{"a", "b", "c"}, sortPost("a", 100), sortPost("b", 100), sortPost("c", 100))},
		{"tie_five", list([]string{"a", "b", "c", "d", "e"},
			sortPost("a", 100), sortPost("b", 100), sortPost("c", 100), sortPost("d", 100), sortPost("e", 100))},
		{"tie_partial", list([]string{"a", "b", "c", "d"},
			sortPost("a", 100), sortPost("b", 200), sortPost("c", 100), sortPost("d", 200))},
		{"negative", list([]string{"a", "b"}, sortPost("a", -5), sortPost("b", 0))},
		// Above 12 elements sort.Slice leaves insertion sort for pdqsort, which is where an
		// unstable algorithm starts to show. These two are the evidence for whether a stable
		// Rust sort still agrees.
		{"tie_thirteen", tiedList(13)},
		{"tie_twenty", tiedList(20)},
		// Two interleaved tie groups above the insertion-sort threshold — the shape a
		// partitioning sort is most likely to shuffle.
		{"tie_interleaved_twenty", interleavedList(20)},
		// An order id with no post: the comparator dereferences nil.
		{"order_without_post", list([]string{"a", "missing"}, sortPost("a", 100))},
	}

	var res []map[string]any
	for _, c := range cases {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			pl := plFromJSON(c.doc)
			pl.SortByCreateAt()
			row["out_order"] = pl.Order
			row["out"] = dumpPostList(pl)
		})
		res = append(res, row)
	}
	return res
}

// --- Etag -------------------------------------------------------------------------------------

// postListEtagAll drives the seed. `id` starts as the string "0" against `t = 0`, so a post with
// update_at 0 competes with it on id alone — "p1" > "0" wins, "!" does not.
func postListEtagAll() []map[string]any {
	etagPost := func(id string, updateAt int64) string {
		return `"` + id + `":{"id":"` + id + `","update_at":` + strconv.FormatInt(updateAt, 10) + `,"channel_id":"c1","user_id":"u1"}`
	}
	list := func(order []string, posts ...string) string {
		return `{"order":` + mustMarshal(order) + `,"posts":{` + strings.Join(posts, ",") + `}}`
	}

	cases := []struct{ name, doc string }{
		{"zero", `{}`},
		{"empty", `{"order":[],"posts":{}}`},
		{"one", list([]string{"p1"}, etagPost("p1", 100))},
		{"no_order", list(nil, etagPost("p1", 100))},
		{"order_only", `{"order":["p1"],"posts":{}}`},
		{"two_distinct", list([]string{"p1", "p2"}, etagPost("p1", 100), etagPost("p2", 200))},
		// The tie-break: equal update_at, so the larger id wins regardless of map order.
		{"tie_on_update_at", list([]string{"p1", "p2"}, etagPost("p1", 100), etagPost("p2", 100))},
		{"tie_reversed_ids", list([]string{"zz", "aa"}, etagPost("zz", 100), etagPost("aa", 100))},
		// update_at 0 competes against the seed id "0" on id alone.
		{"zero_update_at", list([]string{"p1"}, etagPost("p1", 0))},
		{"id_below_zero_char", list([]string{"!"}, etagPost("!", 0))},
		{"negative_update_at", list([]string{"p1"}, etagPost("p1", -5))},
		// The first order entry is the first etag part and need not name a post at all.
		{"order_head_is_a_ghost", list([]string{"ghost", "p1"}, etagPost("p1", 100))},
	}

	var res []map[string]any
	for _, c := range cases {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			row["out"] = plFromJSON(c.doc).Etag()
		})
		res = append(res, row)
	}
	return res
}

// --- IsChannelId ------------------------------------------------------------------------------

func postListIsChannelIDAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"zero", `{}`},
		{"empty_posts", `{"order":[],"posts":{}}`},
		{"one_matching", `{"order":["p1"],"posts":{"p1":` + plPost1 + `}}`},
		{"two_matching", `{"order":["p1","p2"],"posts":{"p1":` + plPost1 + `,"p2":` + plPost2 + `}}`},
		{"mixed", `{"order":["p1","p3"],"posts":{"p1":` + plPost1 + `,"p3":` + plPost3 + `}}`},
	}
	ids := []string{"c1", "c2", ""}

	var res []map[string]any
	for _, d := range docs {
		for _, id := range ids {
			row := map[string]any{"name": d.name + "_" + id, "in": d.doc, "channel_id": id}
			probe(row, func() {
				row["out"] = plFromJSON(d.doc).IsChannelId(id)
			})
			res = append(res, row)
		}
	}
	return res
}

// --- BuildWranglerPostList ----------------------------------------------------------------------

// postListWranglerAll records `list_after` because BuildWranglerPostList reorders and
// deduplicates its receiver before reading it — a caller's list is not the list it passed in.
//
// WranglerPostList carries no json tags at all, so the recorded keys are the Go field names
// verbatim, "EarlistPostTimestamp" typo included.
func postListWranglerAll() []map[string]any {
	wPost := func(id, userID string, createAt int64, fileIds ...string) string {
		return `"` + id + `":{"id":"` + id + `","user_id":"` + userID + `","create_at":` + strconv.FormatInt(createAt, 10) +
			`,"update_at":` + strconv.FormatInt(createAt, 10) + `,"channel_id":"c1","file_ids":` + mustMarshal(fileIds) + `}`
	}
	list := func(order []string, posts ...string) string {
		return `{"order":` + mustMarshal(order) + `,"posts":{` + strings.Join(posts, ",") + `}}`
	}

	cases := []struct{ name, doc string }{
		{"zero", `{}`},
		{"empty", `{"order":[],"posts":{}}`},
		// Order without posts: ToSlice returns a one-element slice holding nil.
		{"order_without_post", `{"order":["ghost"],"posts":{}}`},
		{"one", list([]string{"a"}, wPost("a", "u1", 100))},
		{"thread", list([]string{"a", "b", "c"},
			wPost("a", "u1", 100, "f1"), wPost("b", "u2", 200), wPost("c", "u1", 300, "f2", "f3"))},
		// Duplicate order entries are removed first, so the counts are not doubled.
		{"duplicate_order", `{"order":["a","a"],"posts":{` + wPost("a", "u1", 100, "f1") + `}}`},
		{"repeated_user", list([]string{"a", "b"}, wPost("a", "u1", 100), wPost("b", "u1", 200))},
		{"unsorted_input", list([]string{"a", "b", "c"},
			wPost("a", "u1", 300), wPost("b", "u2", 100), wPost("c", "u3", 200))},
	}

	var res []map[string]any
	for _, c := range cases {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			pl := plFromJSON(c.doc)
			wpl := pl.BuildWranglerPostList()
			row["out"] = mustMarshal(wpl)
			row["num_posts"] = wpl.NumPosts()
			row["contains_file_attachments"] = wpl.ContainsFileAttachments()
			row["root_post_nil"] = wpl.RootPost() == nil
			row["list_after"] = dumpPostList(pl)
		})
		res = append(res, row)
	}
	return res
}

// --- ForPlugin --------------------------------------------------------------------------------

func postForPluginAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"zero", `{}`},
		{"ordinary", plPost1},
		{"with_metadata", `{"id":"p1","message":"m","metadata":{"emojis":[{"name":"a"}]}}`},
		// The one type that also drops a prop.
		{"up_notification", `{"id":"p1","type":"custom_up_notification","props":{"requested_features":{"a":1},"keep":"yes"}}`},
		{"up_notification_no_props", `{"id":"p1","type":"custom_up_notification"}`},
		{"other_custom_type", `{"id":"p1","type":"custom_other","props":{"requested_features":{"a":1}}}`},
	}

	var res []map[string]any
	for _, d := range docs {
		row := map[string]any{"name": d.name, "in": d.doc}
		probe(row, func() {
			row["out"] = mustMarshal(postFromJSON(d.doc).ForPlugin())
		})
		res = append(res, row)
	}
	return res
}

func postListForPluginAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"zero", `{}`},
		{"empty", `{"order":[],"posts":{}}`},
		{"with_metadata", `{"order":["p1"],"posts":{"p1":{"id":"p1","message":"m","channel_id":"c1","user_id":"u1","metadata":{"emojis":[{"name":"a"}]}}}}`},
		{"up_notification", `{"order":["p1"],"posts":{"p1":{"id":"p1","type":"custom_up_notification","channel_id":"c1","user_id":"u1","props":{"requested_features":{"a":1},"keep":"yes"}}}}`},
		{"mixed", `{"order":["p1","p2"],"posts":{"p1":` + plPost1 + `,"p2":` + plPost2 + `}}`},
	}

	var res []map[string]any
	for _, d := range docs {
		row := map[string]any{"name": d.name, "in": d.doc}
		probe(row, func() {
			row["out"] = dumpPostList(plFromJSON(d.doc).ForPlugin())
		})
		res = append(res, row)
	}
	return res
}
