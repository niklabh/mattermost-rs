package main

// Behavioural oracle for mm_blocks_actions.go and (*Post).GetAction. Written to
// fixtures/behaviour_mm_blocks_actions.json.
//
// The whole file is coercion out of an untyped map[string]any inside Post.Props, so **every type
// mismatch is a silent nil** — the same hazard post_interactive_blocks.go has, and the reason a
// reading of the source produces confident wrong answers here. "No such action" is a legitimate
// result, so a port that returns it for the wrong reason looks correct.
//
// Four things need Go's own answer:
//
//  1. **Which malformed entries are misses and which are half-specs.** An entry with no `type` is
//     nil, an entry with an unknown `type` is nil, and an `external` entry with no `url` is a
//     *spec* whose URL is empty — which GetAction then rejects and ResolveMmBlocksAction reports
//     as not-found. Three routes to nil, and only one of them is the same nil.
//
//  2. **MmBlocksContextMap's fallback.** `null`, `[1,2]` and `"a string"` all end up wrapped
//     under the key `context` rather than decoded, each for a different reason; `{}` does not.
//
//  3. **GetAction synthesises a PostAction, and the synthesis is wire surface** — it is what the
//     click pipeline dispatches. Recorded as the marshalled action, so a field-shape drift and a
//     logic drift fail the same test.
//
//  4. **MergeQueryIntoURL's empty-map short circuit.** With no query to merge the input is
//     returned verbatim; with one key the whole URL is re-encoded by url.String(). The
//     merge_query_into_url section of behaviour_go_url.json drives that directly; here it shows
//     up as the difference between a spec with a query and one without.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeMmBlocksActionsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"context_map":        mmBlocksContextMapAll(),
		"entry_to_spec":      mmBlocksEntryToSpecAll(),
		"get_action":         mmBlocksGetActionAll(),
		"cookie_action_spec": mmBlocksCookieActionSpecAll(),
		"resolve":            mmBlocksResolveAll(),
		"parse_cookie":       mmBlocksParseCookieAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_mm_blocks_actions.json"), append(blob, '\n'), 0o644)
}

// --- MmBlocksContextMap ------------------------------------------------------------------------

type mmBlocksContextCase struct {
	Name string `json:"name"`
	In   string `json:"in"`
	// Nil records the nil-vs-empty distinction the JSON below cannot.
	Nil bool   `json:"nil"`
	Out string `json:"out"`
}

func mmBlocksContextMapAll() []mmBlocksContextCase {
	cases := []struct{ name, in string }{
		{"empty", ""},
		{"object", `{"a":1,"b":"c"}`},
		{"empty_object", `{}`},
		{"nested", `{"a":{"b":[1,2]}}`},
		{"json_null", `null`},
		{"json_array", `[1,2]`},
		{"json_string", `"a string"`},
		{"json_number", `7`},
		{"json_bool", `true`},
		{"not_json", `nope`},
		{"trailing_data", `{"a":1} junk`},
		{"whitespace", `   `},
		{"object_with_spaces", `  {"a":1}  `},
	}

	res := make([]mmBlocksContextCase, 0, len(cases))
	for _, c := range cases {
		got := model.MmBlocksContextMap(c.in)
		blob, err := json.Marshal(got)
		if err != nil {
			panic(err)
		}
		res = append(res, mmBlocksContextCase{
			Name: c.name, In: c.in, Nil: got == nil, Out: string(blob),
		})
	}
	return res
}

// --- GetMmBlocksActionSpec / mmBlocksEntryMapToSpec ---------------------------------------------

type mmBlocksSpecCase struct {
	Name     string `json:"name"`
	Post     string `json:"post"`
	ActionID string `json:"action_id"`
	Nil      bool   `json:"nil"`

	Type       string            `json:"type"`
	URL        string            `json:"url"`
	Query      map[string]string `json:"query"`
	QueryNil   bool              `json:"query_nil"`
	Context    string            `json:"context"`
	ContextNil bool              `json:"context_nil"`
}

// mmBlocksSpecCorpus is shared by the spec, get_action and resolve sections. Each entry is the
// JSON a producer would have written into props.
var mmBlocksSpecCorpus = []struct{ name, post, actionID string }{
	{"props_absent", `{"id":"pppppppppppppppppppppppppp"}`, "a1"},
	{"registry_absent", `{"props":{"attachments":[]}}`, "a1"},
	{"registry_null", `{"props":{"mm_blocks_actions":null}}`, "a1"},
	{"registry_string", `{"props":{"mm_blocks_actions":"AAAAcookie"}}`, "a1"},
	{"registry_array", `{"props":{"mm_blocks_actions":[1,2]}}`, "a1"},
	{"registry_number", `{"props":{"mm_blocks_actions":7}}`, "a1"},
	{"empty_action_id", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com"}}}}`, ""},
	{"unknown_action_id", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com"}}}}`, "nope"},
	{"entry_null", `{"props":{"mm_blocks_actions":{"a1":null}}}`, "a1"},
	{"entry_string", `{"props":{"mm_blocks_actions":{"a1":"nope"}}}`, "a1"},
	{"entry_array", `{"props":{"mm_blocks_actions":{"a1":[1]}}}`, "a1"},
	{"entry_empty_object", `{"props":{"mm_blocks_actions":{"a1":{}}}}`, "a1"},

	{"no_type", `{"props":{"mm_blocks_actions":{"a1":{"url":"https://x.example.com"}}}}`, "a1"},
	{"empty_type", `{"props":{"mm_blocks_actions":{"a1":{"type":"","url":"https://x.example.com"}}}}`, "a1"},
	{"unknown_type", `{"props":{"mm_blocks_actions":{"a1":{"type":"nope","url":"https://x.example.com"}}}}`, "a1"},
	{"type_wrong_case", `{"props":{"mm_blocks_actions":{"a1":{"type":"External","url":"https://x.example.com"}}}}`, "a1"},
	{"type_openurl_wrong_case", `{"props":{"mm_blocks_actions":{"a1":{"type":"openurl","url":"https://x.example.com"}}}}`, "a1"},
	{"type_not_a_string", `{"props":{"mm_blocks_actions":{"a1":{"type":7,"url":"https://x.example.com"}}}}`, "a1"},

	{"external_minimal", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h"}}}}`, "a1"},
	{"external_no_url", `{"props":{"mm_blocks_actions":{"a1":{"type":"external"}}}}`, "a1"},
	{"external_url_not_a_string", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":7}}}}`, "a1"},
	{"external_with_query", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","query":{"k":"v","z":"1"}}}}}`, "a1"},
	{"external_query_empty", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","query":{}}}}}`, "a1"},
	{"external_query_mixed_types", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","query":{"k":"v","n":7,"b":true}}}}}`, "a1"},
	{"external_query_all_non_strings", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","query":{"n":7}}}}}`, "a1"},
	{"external_query_not_a_map", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","query":"k=v"}}}}`, "a1"},
	{"external_context_object", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","context":{"secret":"s","n":3}}}}}`, "a1"},
	{"external_context_string_json", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","context":"{\"a\":1}"}}}}`, "a1"},
	{"external_context_string_plain", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","context":"plain"}}}}`, "a1"},
	{"external_context_empty_string", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","context":""}}}}`, "a1"},
	{"external_context_null", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","context":null}}}}`, "a1"},
	{"external_context_array", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","context":[1]}}}}`, "a1"},
	{"external_context_number", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h","context":7}}}}`, "a1"},
	{"external_bad_url", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://a[b/","query":{"k":"v"}}}}}`, "a1"},
	{"external_relative_url", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"/plugins/com.example/h","query":{"k":"v"}}}}}`, "a1"},
	{"external_url_with_query", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h?a=1","query":{"k":"v"}}}}}`, "a1"},
	{"external_query_overrides_url", `{"props":{"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h?k=old","query":{"k":"new"}}}}}`, "a1"},

	{"openurl_minimal", `{"props":{"mm_blocks_actions":{"a1":{"type":"openURL","url":"https://x.example.com/h"}}}}`, "a1"},
	{"openurl_with_context", `{"props":{"mm_blocks_actions":{"a1":{"type":"openURL","url":"https://x.example.com/h","context":{"a":1}}}}}`, "a1"},
	{"openurl_with_query", `{"props":{"mm_blocks_actions":{"a1":{"type":"openURL","url":"https://x.example.com/h","query":{"k":"v"}}}}}`, "a1"},
	{"openurl_no_url", `{"props":{"mm_blocks_actions":{"a1":{"type":"openURL"}}}}`, "a1"},

	// An attachment action wins over the registry, and matches on an exact id.
	{"attachment_action_wins", `{"props":{"attachments":[{"actions":[{"id":"a1","name":"n","integration":{"url":"https://att.example.com"}}]}],` +
		`"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h"}}}}`, "a1"},
	{"attachment_action_other_id", `{"props":{"attachments":[{"actions":[{"id":"other","name":"n"}]}],` +
		`"mm_blocks_actions":{"a1":{"type":"external","url":"https://x.example.com/h"}}}}`, "a1"},
	{"attachment_action_empty_id", `{"props":{"attachments":[{"actions":[{"name":"n"}]}]}}`, ""},
}

func mmBlocksEntryToSpecAll() []mmBlocksSpecCase {
	res := make([]mmBlocksSpecCase, 0, len(mmBlocksSpecCorpus))
	for _, c := range mmBlocksSpecCorpus {
		spec := postFromJSON(c.post).GetMmBlocksActionSpec(c.actionID)
		res = append(res, mmBlocksRecordSpec(c.name, c.post, c.actionID, spec))
	}
	return res
}

func mmBlocksRecordSpec(name, post, actionID string, spec *model.MmBlocksActionSpec) mmBlocksSpecCase {
	out := mmBlocksSpecCase{Name: name, Post: post, ActionID: actionID, Nil: spec == nil}
	if spec == nil {
		return out
	}
	blob, err := json.Marshal(spec.Context)
	if err != nil {
		panic(err)
	}
	out.Type = spec.Type
	out.URL = spec.URL
	out.Query = spec.Query
	out.QueryNil = spec.Query == nil
	out.Context = string(blob)
	out.ContextNil = spec.Context == nil
	return out
}

// --- GetAction ----------------------------------------------------------------------------------

type mmBlocksGetActionCase struct {
	Name     string `json:"name"`
	Post     string `json:"post"`
	ActionID string `json:"action_id"`
	Nil      bool   `json:"nil"`
	// Action is the marshalled *PostAction, which for a synthesised one is wire surface.
	Action string `json:"action"`
}

func mmBlocksGetActionAll() []mmBlocksGetActionCase {
	res := make([]mmBlocksGetActionCase, 0, len(mmBlocksSpecCorpus))
	for _, c := range mmBlocksSpecCorpus {
		got := postFromJSON(c.post).GetAction(c.actionID)
		blob, err := json.Marshal(got)
		if err != nil {
			panic(err)
		}
		res = append(res, mmBlocksGetActionCase{
			Name: c.name, Post: c.post, ActionID: c.actionID,
			Nil: got == nil, Action: string(blob),
		})
	}
	return res
}

// --- MmBlocksActionCookie.ActionSpec --------------------------------------------------------------

type mmBlocksCookieSpecCase struct {
	Name     string `json:"name"`
	Cookie   string `json:"cookie"`
	ActionID string `json:"action_id"`
	Nil      bool   `json:"nil"`
	Type     string `json:"type"`
	URL      string `json:"url"`
}

func mmBlocksCookieActionSpecAll() []mmBlocksCookieSpecCase {
	cases := []struct{ name, cookie, actionID string }{
		{"nil_actions", `{"kind":"mm_blocks_actions"}`, "a1"},
		{"empty_actions", `{"kind":"mm_blocks_actions","actions":{}}`, "a1"},
		{"empty_action_id", `{"actions":{"a1":{"type":"external","url":"https://x.example.com"}}}`, ""},
		{"unknown_action_id", `{"actions":{"a1":{"type":"external","url":"https://x.example.com"}}}`, "nope"},
		{"external", `{"actions":{"a1":{"type":"external","url":"https://x.example.com"}}}`, "a1"},
		{"openurl", `{"actions":{"a1":{"type":"openURL","url":"https://x.example.com"}}}`, "a1"},
		{"no_type", `{"actions":{"a1":{"url":"https://x.example.com"}}}`, "a1"},
	}

	res := make([]mmBlocksCookieSpecCase, 0, len(cases))
	for _, c := range cases {
		var cookie model.MmBlocksActionCookie
		if err := json.Unmarshal([]byte(c.cookie), &cookie); err != nil {
			panic(err)
		}
		spec := cookie.ActionSpec(c.actionID)
		out := mmBlocksCookieSpecCase{
			Name: c.name, Cookie: c.cookie, ActionID: c.actionID, Nil: spec == nil,
		}
		if spec != nil {
			out.Type = spec.Type
			out.URL = spec.URL
		}
		res = append(res, out)
	}
	return res
}

// --- ResolveMmBlocksAction -------------------------------------------------------------------------

type mmBlocksResolveCase struct {
	Name        string            `json:"name"`
	Spec        string            `json:"spec"`
	SpecNil     bool              `json:"spec_nil"`
	ActionID    string            `json:"action_id"`
	ClientQuery map[string]string `json:"client_query"`
	Err         string            `json:"err"`
	Ok          bool              `json:"ok"`
	OpenURLGoto string            `json:"open_url_goto"`
	ExternalURL string            `json:"external_url"`
	Context     string            `json:"context"`
}

func mmBlocksResolveAll() []mmBlocksResolveCase {
	type input struct {
		name        string
		spec        *model.MmBlocksActionSpec
		clientQuery map[string]string
	}

	inputs := []input{
		{"nil_spec", nil, nil},
		{"unknown_type", &model.MmBlocksActionSpec{Type: "nope", URL: "https://x.example.com"}, nil},
		{"empty_type", &model.MmBlocksActionSpec{URL: "https://x.example.com"}, nil},
		{"openurl_no_url", &model.MmBlocksActionSpec{Type: "openURL"}, nil},
		{"openurl", &model.MmBlocksActionSpec{Type: "openURL", URL: "https://x.example.com/h"}, nil},
		{"openurl_with_query", &model.MmBlocksActionSpec{
			Type: "openURL", URL: "https://x.example.com/h", Query: map[string]string{"k": "v"},
		}, nil},
		{"openurl_ignores_client_query", &model.MmBlocksActionSpec{
			Type: "openURL", URL: "https://x.example.com/h",
		}, map[string]string{"c": "1"}},
		{"external_no_url", &model.MmBlocksActionSpec{Type: "external"}, nil},
		{"external", &model.MmBlocksActionSpec{Type: "external", URL: "https://x.example.com/h"}, nil},
		{"external_static_query", &model.MmBlocksActionSpec{
			Type: "external", URL: "https://x.example.com/h", Query: map[string]string{"s": "1"},
		}, nil},
		{"external_client_query", &model.MmBlocksActionSpec{
			Type: "external", URL: "https://x.example.com/h",
		}, map[string]string{"c": "1"}},
		{"client_query_overrides_static", &model.MmBlocksActionSpec{
			Type: "external", URL: "https://x.example.com/h", Query: map[string]string{"k": "static"},
		}, map[string]string{"k": "client"}},
		{"external_with_context", &model.MmBlocksActionSpec{
			Type: "external", URL: "https://x.example.com/h",
			Context: map[string]any{"secret": "s"},
		}, nil},
		{"external_bad_url", &model.MmBlocksActionSpec{
			Type: "external", URL: "https://a[b/", Query: map[string]string{"k": "v"},
		}, nil},
		{"external_bad_url_no_query", &model.MmBlocksActionSpec{
			Type: "external", URL: "https://a[b/",
		}, nil},
	}

	res := make([]mmBlocksResolveCase, 0, len(inputs))
	for _, in := range inputs {
		specBlob, err := json.Marshal(in.spec)
		if err != nil {
			panic(err)
		}
		got, resolveErr := model.ResolveMmBlocksAction(in.spec, "act-1", in.clientQuery)
		out := mmBlocksResolveCase{
			Name: in.name, Spec: string(specBlob), SpecNil: in.spec == nil,
			ActionID: "act-1", ClientQuery: in.clientQuery,
			Err: errString(resolveErr), Ok: resolveErr == nil,
		}
		if got != nil {
			ctx, err := json.Marshal(got.Context)
			if err != nil {
				panic(err)
			}
			out.OpenURLGoto = got.OpenURLGoto
			out.ExternalURL = got.ExternalURL
			out.Context = string(ctx)
		}
		res = append(res, out)
	}
	return res
}

// --- ParseDecryptedActionCookiePayload ---------------------------------------------------------

type mmBlocksParseCookieCase struct {
	Name        string `json:"name"`
	In          string `json:"in"`
	Ok          bool   `json:"ok"`
	Err         string `json:"err"`
	LegacyNil   bool   `json:"legacy_nil"`
	MmBlocksNil bool   `json:"mm_blocks_nil"`
	Legacy      string `json:"legacy"`
	MmBlocks    string `json:"mm_blocks"`
}

func mmBlocksParseCookieAll() []mmBlocksParseCookieCase {
	cases := []struct{ name, in string }{
		{"legacy_full", `{"type":"button","post_id":"p","root_id":"r","channel_id":"c","data_source":"users"}`},
		{"legacy_empty_object", `{}`},
		{"legacy_unknown_kind", `{"kind":"something_else","post_id":"p"}`},
		{"mm_blocks", `{"kind":"mm_blocks_actions","post_id":"p","actions":{"a1":{"type":"external"}}}`},
		{"mm_blocks_no_actions", `{"kind":"mm_blocks_actions"}`},
		{"kind_wrong_case", `{"kind":"MM_BLOCKS_ACTIONS"}`},
		{"kind_not_a_string", `{"kind":7}`},
		{"json_null", `null`},
		{"json_number", `7`},
		{"json_array", `[]`},
		{"json_string", `"nope"`},
		{"not_json", `nope`},
		{"empty", ``},
	}

	res := make([]mmBlocksParseCookieCase, 0, len(cases))
	for _, c := range cases {
		legacy, mm, err := model.ParseDecryptedActionCookiePayload(c.in)
		legacyBlob, e := json.Marshal(legacy)
		if e != nil {
			panic(e)
		}
		mmBlob, e := json.Marshal(mm)
		if e != nil {
			panic(e)
		}
		res = append(res, mmBlocksParseCookieCase{
			Name: c.name, In: c.in, Ok: err == nil, Err: errString(err),
			LegacyNil: legacy == nil, MmBlocksNil: mm == nil,
			Legacy: string(legacyBlob), MmBlocks: string(mmBlob),
		})
	}
	return res
}
