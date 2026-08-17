package main

// Behavioural oracle for integration_action.go chunk 3 — the three *Post* methods that walk
// props.attachments — plus the two Post serialisers they unblock and the one mm_blocks_actions.go
// helper StripActionIntegrations depends on. Written to fixtures/behaviour_post_actions.json.
//
// Five things need Go's own answer here:
//
//  1. **StripActionIntegrations rewrites props.attachments, it does not only edit it.** It stores
//     the []*MessageAttachment that Attachments() decoded, so every unknown key, every wrongly
//     typed element and every nil action in the client's payload is normalised away by the round
//     trip. What survives that rewrite is the whole point, and it is not visible in the source.
//
//  2. **The rewrite is conditional and the mutation is not.** AddProp runs only when the prop is
//     non-nil, while the Integration-nilling loop runs over the decoded slice regardless. Go gets
//     away with storing first and mutating afterwards because the slice is aliased; a port that
//     owns its values has to invert the order, so the corpus pins the result rather than the
//     sequence.
//
//  3. **ToJSON clones and EncodeJSON does not.** One leaves the receiver's integrations in place
//     and the other strips them permanently. Each case records the receiver *after* the call
//     alongside the output so the asymmetry cannot be read the wrong way round.
//
//  4. **GenerateActionIds mints ids with NewId(), which is a CSPRNG.** Recording the raw output
//     would break the determinism rule in main.go's header ([D-032]), so every id absent from the
//     input is replaced with the literal "<generated>" and counted. What is being pinned is which
//     actions get an id, not which id they get.
//
//  5. **DelProp materialises a nil Props into an empty map**, because it builds a fresh map and
//     assigns it unconditionally. Props has no omitempty, so that turns `"props":null` into
//     `"props":{}` on the wire — reachable through StripMmBlocksActionSecrets and worth its own
//     probe.

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

// generatedIDPlaceholder replaces any action id GenerateActionIds minted, so the fixture stays
// byte-identical across runs. See note 4 above.
const generatedIDPlaceholder = "<generated>"

func writePostActionsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"strip_action_integrations":      postStripActionIntegrationsAll(),
		"strip_mm_blocks_action_secrets": postStripMmBlocksActionSecretsAll(),
		"generate_action_ids":            postGenerateActionIdsAll(),
		"pre_commit":                     postActionsPreCommitAll(),
		"to_json":                        postToJSONAll(),
		"encode_json":                    postEncodeJSONAll(),
		"del_prop":                       postDelPropAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_post_actions.json"), append(blob, '\n'), 0o644)
}

// marshalPost renders a post exactly as the wire does. Errors panic: a post that cannot be
// marshalled is a corpus bug, not a result worth recording.
func marshalPost(p *model.Post) string {
	blob, err := json.Marshal(p)
	if err != nil {
		panic(err)
	}
	return string(blob)
}

// --- the shared corpus ------------------------------------------------------------------------

// postActionCorpus is driven through several of the sections below. Each entry is the JSON a
// client could send, so the decode path under test is the one a real request takes.
var postActionCorpus = []struct{ name, post string }{
	// Nothing to walk. The first three differ only in how props is spelled, which is the
	// difference between GetProp returning nil and returning a value.
	{"props_absent", `{"id":"pppppppppppppppppppppppppp","message":"hi"}`},
	{"props_null", `{"id":"pppppppppppppppppppppppppp","props":null}`},
	{"props_empty", `{"id":"pppppppppppppppppppppppppp","props":{}}`},
	{"attachments_null", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":null}}`},
	{"attachments_empty_array", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[]}}`},

	// props.attachments present but not an array: Attachments() returns nil and the prop is
	// nonetheless overwritten with that nil slice.
	{"attachments_string", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":"nope"}}`},
	{"attachments_object", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":{"a":1}}}`},
	{"attachments_number", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":7}}`},

	// No actions at all — the rewrite still normalises the attachment.
	{"attachment_no_actions", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"title":"t","text":"x"}]}}`},
	{"attachment_unknown_keys", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"title":"t","nope":1,"deep":{"a":[1,2]}}]}}`},
	{"attachment_bad_type_dropped", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"title":123},{"title":"kept"}]}}`},
	{"attachment_null_element", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[null]}}`},

	// One action carrying an integration. This is the case the whole function exists for.
	{"one_action_with_integration", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"title":"t",` +
		`"actions":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"n","type":"button",` +
		`"integration":{"url":"https://example.com/x","context":{"secret":"s","n":3}}}]}]}}`},
	{"action_integration_empty_object", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"title":"t",` +
		`"actions":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"n","integration":{}}]}]}}`},
	{"action_integration_null", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"title":"t",` +
		`"actions":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"n","integration":null}]}]}}`},
	{"action_no_integration", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"title":"t",` +
		`"actions":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"n"}]}]}}`},

	// Two attachments, several actions, mixed ids — the shape GenerateActionIds is written for.
	{"two_attachments_mixed_ids", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[` +
		`{"title":"a","actions":[{"name":"n1","integration":{"url":"https://a.example.com"}},` +
		`{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"n2","integration":{"url":"https://b.example.com"}}]},` +
		`{"title":"b","actions":[{"name":"n3"}]}]}}`},
	{"action_id_blank_string", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"actions":[{"id":"","name":"n"}]}]}}`},
	{"action_id_whitespace", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"actions":[{"id":"  ","name":"n"}]}]}}`},
	{"action_id_short", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"actions":[{"id":"x","name":"n"}]}]}}`},

	// Go strips nil actions before returning, so the surviving action is still processed.
	{"actions_null_then_real", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"actions":[null,{"name":"n"}]}]}}`},
	{"actions_all_null", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"actions":[null,null]}]}}`},

	// A select action with options, to prove the rest of the action survives the nilling.
	{"select_action_with_options", `{"id":"pppppppppppppppppppppppppp","props":{"attachments":[{"actions":[` +
		`{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"n","type":"select","options":[{"text":"t1","value":"v1"}],` +
		`"integration":{"url":"https://example.com/x","context":{"k":"v"}}}]}]}}`},

	// Other props must survive untouched, in particular the reserved ones.
	{"other_props_survive", `{"id":"pppppppppppppppppppppppppp","props":{"from_webhook":"true","override_username":"bot",` +
		`"attachments":[{"actions":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"n","integration":{"url":"https://x.example.com"}}]}]}}`},

	// mm_blocks_actions alongside attachments, in both of its two reachable shapes.
	{"mm_blocks_actions_map", `{"id":"pppppppppppppppppppppppppp","props":{"mm_blocks_actions":{"a1":{"type":"external",` +
		`"url":"https://example.com/hook","query":{"k":"v"},"context":{"secret":"s"}}}}}`},
	{"mm_blocks_actions_cookie_string", `{"id":"pppppppppppppppppppppppppp","props":{"mm_blocks_actions":"AAAAencrypted"}}`},
	{"mm_blocks_actions_null", `{"id":"pppppppppppppppppppppppppp","props":{"mm_blocks_actions":null}}`},
	{"mm_blocks_actions_empty_map", `{"id":"pppppppppppppppppppppppppp","props":{"mm_blocks_actions":{}}}`},
	{"mm_blocks_actions_number", `{"id":"pppppppppppppppppppppppppp","props":{"mm_blocks_actions":7}}`},
	{"mm_blocks_actions_array", `{"id":"pppppppppppppppppppppppppp","props":{"mm_blocks_actions":[1,2]}}`},
	{"mm_blocks_actions_empty_string", `{"id":"pppppppppppppppppppppppppp","props":{"mm_blocks_actions":""}}`},
	{"mm_blocks_and_attachments", `{"id":"pppppppppppppppppppppppppp","props":{"mm_blocks_actions":{"a1":{"type":"openURL","url":"https://e.example.com"}},` +
		`"attachments":[{"actions":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"n","integration":{"url":"https://x.example.com"}}]}]}}`},

	// Characters encoding/json HTML-escapes, with no attachments in the way — so the recorded
	// bytes can be asserted byte-for-byte and prove which marshaller the Rust port must use.
	{"html_escaping_no_attachments", `{"id":"pppppppppppppppppppppppppp","message":"a<b>&c",` +
		`"props":{"note":"x<y>&z","sep":"a b c"}}`},

	// The same characters with an attachment present, which is where the field-order divergence
	// between a Go struct and a serde_json::Map shows up.
	{"html_escaping_in_props", `{"id":"pppppppppppppppppppppppppp","props":{"note":"a<b>c&d",` +
		`"attachments":[{"title":"<b>","actions":[{"id":"aaaaaaaaaaaaaaaaaaaaaaaaaa","name":"<n>","integration":{"url":"https://x.example.com?a=1&b=2"}}]}]}}`},
}

// --- StripActionIntegrations ------------------------------------------------------------------

type postStripCase struct {
	Name string `json:"name"`
	Post string `json:"post"`
	Out  string `json:"out"`
}

func postStripActionIntegrationsAll() []postStripCase {
	res := make([]postStripCase, 0, len(postActionCorpus))
	for _, c := range postActionCorpus {
		p := postFromJSON(c.post)
		p.StripActionIntegrations()
		res = append(res, postStripCase{Name: c.name, Post: c.post, Out: marshalPost(p)})
	}
	return res
}

// --- StripMmBlocksActionSecrets (mm_blocks_actions.go:243) -------------------------------------

func postStripMmBlocksActionSecretsAll() []postStripCase {
	res := make([]postStripCase, 0, len(postActionCorpus))
	for _, c := range postActionCorpus {
		p := postFromJSON(c.post)
		p.StripMmBlocksActionSecrets()
		res = append(res, postStripCase{Name: c.name, Post: c.post, Out: marshalPost(p)})
	}
	return res
}

// --- GenerateActionIds / PreCommit -------------------------------------------------------------

type postGenerateIDsCase struct {
	Name string `json:"name"`
	Post string `json:"post"`
	// Out is the marshalled post with every minted id replaced by generatedIDPlaceholder.
	Out            string `json:"out"`
	GeneratedCount int    `json:"generated_count"`
}

// knownActionIDs is the set of action ids already present before the call, so anything else in
// the output was minted by NewId().
func knownActionIDs(p *model.Post) map[string]bool {
	known := map[string]bool{}
	for _, attachment := range p.Attachments() {
		for _, action := range attachment.Actions {
			if action != nil && action.Id != "" {
				known[action.Id] = true
			}
		}
	}
	return known
}

// blankGeneratedIDs rewrites the post's stored attachments in place. It is called after the
// method under test, so props.attachments already holds the native []*MessageAttachment slice.
func blankGeneratedIDs(p *model.Post, known map[string]bool) int {
	attachments, ok := p.GetProp(model.PostPropsAttachments).([]*model.MessageAttachment)
	if !ok {
		return 0
	}
	count := 0
	for _, attachment := range attachments {
		for _, action := range attachment.Actions {
			if action != nil && action.Id != "" && !known[action.Id] {
				action.Id = generatedIDPlaceholder
				count++
			}
		}
	}
	return count
}

func postGenerateActionIdsAll() []postGenerateIDsCase {
	res := make([]postGenerateIDsCase, 0, len(postActionCorpus))
	for _, c := range postActionCorpus {
		p := postFromJSON(c.post)
		known := knownActionIDs(p)
		p.GenerateActionIds()
		count := blankGeneratedIDs(p, known)
		res = append(res, postGenerateIDsCase{
			Name:           c.name,
			Post:           c.post,
			Out:            marshalPost(p),
			GeneratedCount: count,
		})
	}
	return res
}

// postActionsPreCommitAll re-runs the corpus through PreCommit, which is where GenerateActionIds is
// actually reached in production. Everything else PreCommit does is deterministic, so the only
// non-determinism is the same minted ids.
func postActionsPreCommitAll() []postGenerateIDsCase {
	res := make([]postGenerateIDsCase, 0, len(postActionCorpus))
	for _, c := range postActionCorpus {
		p := postFromJSON(c.post)
		known := knownActionIDs(p)
		p.PreCommit()
		count := blankGeneratedIDs(p, known)
		res = append(res, postGenerateIDsCase{
			Name:           c.name,
			Post:           c.post,
			Out:            marshalPost(p),
			GeneratedCount: count,
		})
	}
	return res
}

// --- ToJSON / EncodeJSON -----------------------------------------------------------------------

type postSerialiseCase struct {
	Name string `json:"name"`
	Post string `json:"post"`
	// Out is the exact output — ToJSON's returned string, or the bytes EncodeJSON wrote,
	// trailing newline included.
	Out string `json:"out"`
	// ReceiverAfter is the receiver marshalled again after the call. ToJSON clones and leaves it
	// alone; EncodeJSON strips in place.
	ReceiverAfter string `json:"receiver_after"`
	Err           string `json:"err"`
}

func postToJSONAll() []postSerialiseCase {
	res := make([]postSerialiseCase, 0, len(postActionCorpus))
	for _, c := range postActionCorpus {
		p := postFromJSON(c.post)
		got, err := p.ToJSON()
		errStr := ""
		if err != nil {
			errStr = err.Error()
		}
		res = append(res, postSerialiseCase{
			Name:          c.name,
			Post:          c.post,
			Out:           got,
			ReceiverAfter: marshalPost(p),
			Err:           errStr,
		})
	}
	return res
}

func postEncodeJSONAll() []postSerialiseCase {
	res := make([]postSerialiseCase, 0, len(postActionCorpus))
	for _, c := range postActionCorpus {
		p := postFromJSON(c.post)
		var buf bytes.Buffer
		err := p.EncodeJSON(&buf)
		errStr := ""
		if err != nil {
			errStr = err.Error()
		}
		res = append(res, postSerialiseCase{
			Name:          c.name,
			Post:          c.post,
			Out:           buf.String(),
			ReceiverAfter: marshalPost(p),
			Err:           errStr,
		})
	}
	return res
}

// --- DelProp on a nil Props ---------------------------------------------------------------------

type postDelPropCase struct {
	Name string `json:"name"`
	Post string `json:"post"`
	Key  string `json:"key"`
	Out  string `json:"out"`
}

// postDelPropAll exists for one line of post.go:749 — DelProp builds a fresh map and assigns it
// unconditionally, so deleting from a *nil* Props leaves an empty map behind. Props has no
// omitempty, so `"props":null` becomes `"props":{}`.
func postDelPropAll() []postDelPropCase {
	cases := []struct{ name, post, key string }{
		{"nil_props", `{"id":"pppppppppppppppppppppppppp"}`, "anything"},
		{"null_props", `{"id":"pppppppppppppppppppppppppp","props":null}`, "anything"},
		{"empty_props", `{"id":"pppppppppppppppppppppppppp","props":{}}`, "anything"},
		{"absent_key", `{"id":"pppppppppppppppppppppppppp","props":{"a":1}}`, "b"},
		{"present_key", `{"id":"pppppppppppppppppppppppppp","props":{"a":1,"b":2}}`, "b"},
		{"last_key", `{"id":"pppppppppppppppppppppppppp","props":{"b":2}}`, "b"},
		{"key_holding_null", `{"id":"pppppppppppppppppppppppppp","props":{"b":null}}`, "b"},
	}

	res := make([]postDelPropCase, 0, len(cases))
	for _, c := range cases {
		p := postFromJSON(c.post)
		p.DelProp(c.key)
		res = append(res, postDelPropCase{Name: c.name, Post: c.post, Key: c.key, Out: marshalPost(p)})
	}
	return res
}
