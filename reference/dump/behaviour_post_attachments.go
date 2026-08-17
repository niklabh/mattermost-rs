package main

// Behavioural oracle for post.go chunk 2 — Attachments, AttachmentsEqual and AllStrings.
// Written to fixtures/behaviour_post_attachments.json.
//
// Four things need Go's own answer here:
//
//  1. **Attachments() is a re-decode, not a cast.** props.attachments arrives from JSON as a
//     []any of map[string]any, and Go marshals each element and unmarshals it into a
//     MessageAttachment, dropping the element when either step fails. Which inputs survive that
//     round trip is not obvious from the source: a bare `null` element does *not* fail, and a
//     wrongly-typed field does. The `attachments` section drives every shape.
//
//  2. **It filters nil actions and nil fields, and only those.** A nil *option* inside an action
//     is kept, so the decoded attachment can hold one — which our Vec<PostActionOptions> cannot
//     ([D-033]). The corpus separates the three cases.
//
//  3. **AttachmentsEqual calls MessageAttachmentField.Equals, which panics on a nil Value**
//     ([D-039]) — and a field with no `value` key decodes to exactly that. So comparing two
//     ordinary posts crashes the Go server. Measured under recover rather than reasoned about.
//
//  4. **AllStrings is asymmetric about trimming.** A string field value is appended with its
//     original bytes when it is not whitespace-only; a non-string value is rendered with
//     fmt.Sprint and appended *trimmed*. The whitespace test is strings.TrimSpace, i.e. Unicode
//     White_Space, so the corpus probes NBSP, ideographic space and zero-width space too.
//
// The interactive-blocks half of AllStrings (post_interactive_blocks.go) is unported, so every
// case is recorded under **both** values of OmitInteractiveBlocks: the pairs that differ are the
// evidence for what is still owed, and the pairs that agree are what the Rust port asserts.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writePostAttachmentsBehaviourFixture(outDir string) error {
	out := map[string]any{
		"attachments":       postAttachmentsAll(),
		"attachments_equal": postAttachmentsEqualAll(),
		"all_strings":       postAllStringsAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_post_attachments.json"), append(blob, '\n'), 0o644)
}

func postFromJSON(blob string) *model.Post {
	var p model.Post
	if err := json.Unmarshal([]byte(blob), &p); err != nil {
		panic(err)
	}
	return &p
}

// --- Attachments ------------------------------------------------------------------------------

type postAttachmentsCase struct {
	Name string `json:"name"`
	Post string `json:"post"`
	// Count is recorded separately from the JSON because a nil slice and an empty one both
	// marshal to something a reader can misread.
	Count       int    `json:"count"`
	Attachments string `json:"attachments"`
}

func postAttachmentsAll() []postAttachmentsCase {
	cases := []struct{ name, post string }{
		{"props_absent", `{"message":"hi"}`},
		{"props_empty", `{"props":{}}`},
		{"attachments_null", `{"props":{"attachments":null}}`},
		{"attachments_empty_array", `{"props":{"attachments":[]}}`},

		// Not an array: the type assertion fails and the result is nil.
		{"attachments_object", `{"props":{"attachments":{"id":1}}}`},
		{"attachments_string", `{"props":{"attachments":"nope"}}`},
		{"attachments_number", `{"props":{"attachments":7}}`},
		{"attachments_bool", `{"props":{"attachments":true}}`},

		// Element shapes. A bare null re-marshals to `null`, which unmarshals into a struct
		// without error — so it survives as a zero attachment rather than being dropped.
		{"element_null", `{"props":{"attachments":[null]}}`},
		{"element_empty_object", `{"props":{"attachments":[{}]}}`},
		{"element_string", `{"props":{"attachments":["nope"]}}`},
		{"element_number", `{"props":{"attachments":[7]}}`},
		{"element_bool", `{"props":{"attachments":[false]}}`},
		{"element_array", `{"props":{"attachments":[[]]}}`},

		{"one_full", `{"props":{"attachments":[{"id":3,"fallback":"f","color":"#a1b2c3",` +
			`"pretext":"p","author_name":"an","author_link":"https://a.example.com",` +
			`"author_icon":"https://i.example.com","title":"t",` +
			`"title_link":"https://t.example.com","text":"x","image_url":"https://im.example.com",` +
			`"thumb_url":"https://th.example.com","footer":"foot",` +
			`"footer_icon":"https://fi.example.com","ts":"1700000000"}]}}`},
		{"unknown_keys_ignored", `{"props":{"attachments":[{"title":"t","nope":1,"deeper":{"a":[1]}}]}}`},
		// encoding/json matches struct fields case-insensitively; serde does not. Crate-wide
		// divergence, measured here because this is where a decode of client data happens.
		{"case_insensitive_keys", `{"props":{"attachments":[{"Title":"t","TEXT":"x"}]}}`},
		{"duplicate_keys_last_wins", `{"props":{"attachments":[{"title":"a","title":"b"}]}}`},

		// A type error anywhere in the element drops the whole element.
		{"bad_field_type_string", `{"props":{"attachments":[{"title":123}]}}`},
		{"bad_field_type_id_float", `{"props":{"attachments":[{"id":1.5}]}}`},
		{"id_integral_float", `{"props":{"attachments":[{"id":3}]}}`},
		{"fields_not_array", `{"props":{"attachments":[{"fields":{}}]}}`},
		{"bad_element_between_good", `{"props":{"attachments":[{"title":"a"},{"title":123},{"title":"b"}]}}`},

		// ts is a bare any: anything decodes, IsValid is what rejects it.
		{"ts_number", `{"props":{"attachments":[{"ts":123}]}}`},
		{"ts_null", `{"props":{"attachments":[{"ts":null}]}}`},

		// short is a SlackCompatibleBool.
		{"field_short_quoted_true", `{"props":{"attachments":[{"fields":[{"title":"t","short":"true"}]}]}}`},
		{"field_short_bad", `{"props":{"attachments":[{"fields":[{"title":"t","short":"yes"}]}]}}`},

		// The nil filters. Actions and fields are stripped of nils; a nil option inside an
		// action is not, which is the [D-033] exposure.
		{"actions_one_null", `{"props":{"attachments":[{"actions":[null]}]}}`},
		{"actions_null_then_real", `{"props":{"attachments":[{"actions":[null,{"id":"a1","name":"n"}]}]}}`},
		{"actions_real_then_null", `{"props":{"attachments":[{"actions":[{"id":"a1","name":"n"},null]}]}}`},
		{"actions_empty", `{"props":{"attachments":[{"actions":[]}]}}`},
		{"fields_one_null", `{"props":{"attachments":[{"fields":[null]}]}}`},
		{"fields_null_then_real", `{"props":{"attachments":[{"fields":[null,{"title":"t","value":"v"}]}]}}`},
		{"fields_empty", `{"props":{"attachments":[{"fields":[]}]}}`},
		{"fields_null", `{"props":{"attachments":[{"fields":null}]}}`},
		{"action_option_null", `{"props":{"attachments":[{"actions":[{"id":"a1","name":"n","options":[null]}]}]}}`},
		{"action_option_null_then_real", `{"props":{"attachments":[{"actions":[{"id":"a1","name":"n",` +
			`"options":[null,{"text":"t","value":"v"}]}]}]}}`},

		// A stored []*MessageAttachment (the native-type branch) is unreachable from JSON; the
		// closest reachable input is the marshalled form of the same value.
		{"two_attachments", `{"props":{"attachments":[{"title":"a"},{"title":"b"}]}}`},
	}

	res := make([]postAttachmentsCase, 0, len(cases))
	for _, c := range cases {
		got := postFromJSON(c.post).Attachments()
		blob, err := json.Marshal(got)
		if err != nil {
			panic(err)
		}
		res = append(res, postAttachmentsCase{
			Name:        c.name,
			Post:        c.post,
			Count:       len(got),
			Attachments: string(blob),
		})
	}
	return res
}

// --- AttachmentsEqual -------------------------------------------------------------------------

type postAttachmentsEqualCase struct {
	Name     string `json:"name"`
	A        string `json:"a"`
	B        string `json:"b"`
	Equal    bool   `json:"equal"`
	Panicked bool   `json:"panicked"`
}

func postAttachmentsEqualAll() []postAttachmentsEqualCase {
	cases := []struct{ name, a, b string }{
		{"both_absent", `{}`, `{}`},
		{"absent_against_empty", `{}`, `{"props":{"attachments":[]}}`},
		{"one_against_none", `{"props":{"attachments":[{"title":"a"}]}}`, `{}`},
		{"different_lengths", `{"props":{"attachments":[{"title":"a"}]}}`,
			`{"props":{"attachments":[{"title":"a"},{"title":"b"}]}}`},
		{"same_title", `{"props":{"attachments":[{"title":"a"}]}}`, `{"props":{"attachments":[{"title":"a"}]}}`},
		{"different_title", `{"props":{"attachments":[{"title":"a"}]}}`, `{"props":{"attachments":[{"title":"b"}]}}`},
		{"different_order", `{"props":{"attachments":[{"title":"a"},{"title":"b"}]}}`,
			`{"props":{"attachments":[{"title":"b"},{"title":"a"}]}}`},

		// ts is compared with == on two anys; both sides came from JSON, so both are float64.
		{"ts_int_against_float", `{"props":{"attachments":[{"ts":1}]}}`, `{"props":{"attachments":[{"ts":1.0}]}}`},
		{"ts_exponent", `{"props":{"attachments":[{"ts":100}]}}`, `{"props":{"attachments":[{"ts":1e2}]}}`},
		{"ts_string_against_number", `{"props":{"attachments":[{"ts":"1"}]}}`, `{"props":{"attachments":[{"ts":1}]}}`},

		// A field with no `value` key is a nil any, and Equals reflects on it.
		{"field_without_value", `{"props":{"attachments":[{"fields":[{"title":"t"}]}]}}`,
			`{"props":{"attachments":[{"fields":[{"title":"t"}]}]}}`},
		{"field_value_explicit_null", `{"props":{"attachments":[{"fields":[{"title":"t","value":null}]}]}}`,
			`{"props":{"attachments":[{"fields":[{"title":"t","value":"v"}]}]}}`},
		{"field_values_equal", `{"props":{"attachments":[{"fields":[{"title":"t","value":"v"}]}]}}`,
			`{"props":{"attachments":[{"fields":[{"title":"t","value":"v"}]}]}}`},
		{"field_values_differ", `{"props":{"attachments":[{"fields":[{"title":"t","value":"v"}]}]}}`,
			`{"props":{"attachments":[{"fields":[{"title":"t","value":"w"}]}]}}`},
		{"field_value_number_equal", `{"props":{"attachments":[{"fields":[{"title":"t","value":2}]}]}}`,
			`{"props":{"attachments":[{"fields":[{"title":"t","value":2.0}]}]}}`},

		// Equals ignores Tooltip, Disabled and Style on actions — see [D-038].
		{"actions_differ_by_style", `{"props":{"attachments":[{"actions":[{"id":"a","name":"n","style":"primary"}]}]}}`,
			`{"props":{"attachments":[{"actions":[{"id":"a","name":"n","style":"danger"}]}]}}`},
		{"actions_differ_by_name", `{"props":{"attachments":[{"actions":[{"id":"a","name":"n"}]}]}}`,
			`{"props":{"attachments":[{"actions":[{"id":"a","name":"m"}]}]}}`},

		// The nil filter runs before the comparison, so a nil action is invisible to it.
		{"nil_action_filtered_before_compare", `{"props":{"attachments":[{"actions":[null,{"id":"a","name":"n"}]}]}}`,
			`{"props":{"attachments":[{"actions":[{"id":"a","name":"n"}]}]}}`},
		{"nil_element_against_empty_object", `{"props":{"attachments":[null]}}`, `{"props":{"attachments":[{}]}}`},

		// A dropped element changes the length, so a malformed attachment is not "equal to
		// nothing" — it is absent.
		{"malformed_against_absent", `{"props":{"attachments":[{"title":123}]}}`, `{}`},
	}

	res := make([]postAttachmentsEqualCase, 0, len(cases))
	for _, c := range cases {
		row := postAttachmentsEqualCase{Name: c.name, A: c.a, B: c.b}
		func() {
			defer func() {
				if r := recover(); r != nil {
					row.Panicked = true
				}
			}()
			row.Equal = postFromJSON(c.a).AttachmentsEqual(postFromJSON(c.b))
			row.Panicked = false
		}()
		res = append(res, row)
	}
	return res
}

// --- AllStrings -------------------------------------------------------------------------------

type postAllStringsCase struct {
	Name string `json:"name"`
	Post string `json:"post"`
	// Recorded under both option values. `omitting` is the one the Rust port reproduces today;
	// `full` additionally walks the interactive blocks, which are unported.
	Omitting []string `json:"omitting"`
	Full     []string `json:"full"`
	Differs  bool     `json:"differs"`
}

func postAllStringsAll() []postAllStringsCase {
	cases := []struct{ name, post string }{
		{"empty_post", `{}`},
		{"message_plain", `{"message":"hello"}`},
		{"message_padded_kept_verbatim", `{"message":"  hello  "}`},
		{"message_empty", `{"message":""}`},
		{"message_spaces", `{"message":"   "}`},
		{"message_tab_newline", `{"message":"\t\n\r\u000b\f"}`},
		{"message_nbsp", `{"message":"\u00a0"}`},
		{"message_ideographic_space", `{"message":"\u3000"}`},
		{"message_zero_width_space", `{"message":"\u200b"}`},
		{"message_ogham_space_mark", `{"message":"\u1680"}`},
		{"message_mongolian_vowel_separator", `{"message":"\u180e"}`},
		{"message_next_line", `{"message":"\u0085"}`},
		{"message_markdown_preserved", `{"message":"# head\n\n- a\n- b\n"}`},

		{"attachment_order", `{"message":"m","props":{"attachments":[{"author_name":"an","title":"ti",` +
			`"text":"tx","pretext":"pre","footer":"fo"}]}}`},
		{"attachment_blank_parts_dropped", `{"props":{"attachments":[{"author_name":"  ","title":"",` +
			`"text":"tx","pretext":"\t","footer":"fo"}]}}`},
		{"attachment_parts_kept_verbatim", `{"props":{"attachments":[{"title":"  padded  "}]}}`},
		{"two_attachments", `{"props":{"attachments":[{"title":"a"},{"title":"b"}]}}`},
		{"malformed_attachment_skipped", `{"props":{"attachments":[{"title":"a"},{"title":123}]}}`},
		{"null_attachment_contributes_nothing", `{"props":{"attachments":[null]}}`},

		// Fields: title, then the value. A string value keeps its bytes; anything else is
		// rendered with fmt.Sprint and trimmed.
		{"field_string_value", `{"props":{"attachments":[{"fields":[{"title":"ft","value":"fv"}]}]}}`},
		{"field_string_value_padded", `{"props":{"attachments":[{"fields":[{"title":"ft","value":"  fv  "}]}]}}`},
		{"field_string_value_blank", `{"props":{"attachments":[{"fields":[{"title":"ft","value":"   "}]}]}}`},
		{"field_value_absent", `{"props":{"attachments":[{"fields":[{"title":"ft"}]}]}}`},
		{"field_value_explicit_null", `{"props":{"attachments":[{"fields":[{"title":"ft","value":null}]}]}}`},
		{"field_value_number", `{"props":{"attachments":[{"fields":[{"title":"ft","value":42}]}]}}`},
		{"field_value_zero", `{"props":{"attachments":[{"fields":[{"title":"ft","value":0}]}]}}`},
		{"field_value_float", `{"props":{"attachments":[{"fields":[{"title":"ft","value":1.5}]}]}}`},
		{"field_value_big_float", `{"props":{"attachments":[{"fields":[{"title":"ft","value":123456789}]}]}}`},
		{"field_value_exponent", `{"props":{"attachments":[{"fields":[{"title":"ft","value":1e6}]}]}}`},
		{"field_value_true", `{"props":{"attachments":[{"fields":[{"title":"ft","value":true}]}]}}`},
		{"field_value_false", `{"props":{"attachments":[{"fields":[{"title":"ft","value":false}]}]}}`},
		{"field_value_object", `{"props":{"attachments":[{"fields":[{"title":"ft","value":{"b":2,"a":1}}]}]}}`},
		{"field_value_array", `{"props":{"attachments":[{"fields":[{"title":"ft","value":["a",1,null]}]}]}}`},
		{"field_value_empty_object", `{"props":{"attachments":[{"fields":[{"title":"ft","value":{}}]}]}}`},
		{"field_value_empty_array", `{"props":{"attachments":[{"fields":[{"title":"ft","value":[]}]}]}}`},
		{"field_value_empty_string", `{"props":{"attachments":[{"fields":[{"title":"ft","value":""}]}]}}`},
		{"field_blank_title_kept_value", `{"props":{"attachments":[{"fields":[{"title":"  ","value":"v"}]}]}}`},
		{"field_null_filtered", `{"props":{"attachments":[{"fields":[null,{"title":"ft","value":"fv"}]}]}}`},
		{"two_fields", `{"props":{"attachments":[{"fields":[{"title":"a","value":"b"},{"title":"c","value":2}]}]}}`},

		// Interactive payloads: the two option values diverge here and nowhere else.
		{"mm_blocks_present", `{"message":"m","props":{"mm_blocks":[{"type":"text","text":"block text"}]}}`},
		{"block_kit_present", `{"message":"m","props":{"blocks":[{"type":"section",` +
			`"text":{"type":"mrkdwn","text":"kit text"}}]}}`},
		{"adaptive_cards_present", `{"message":"m","props":{"cards":[{"type":"AdaptiveCard",` +
			`"body":[{"type":"TextBlock","text":"card text"}]}]}}`},
		{"mm_blocks_empty_array", `{"message":"m","props":{"mm_blocks":[]}}`},
		{"mm_blocks_not_an_array", `{"message":"m","props":{"mm_blocks":"nope"}}`},
		{"attachments_and_mm_blocks", `{"message":"m","props":{"attachments":[{"title":"a"}],` +
			`"mm_blocks":[{"type":"text","text":"block text"}]}}`},
	}

	res := make([]postAllStringsCase, 0, len(cases))
	for _, c := range cases {
		omitting := postFromJSON(c.post).AllStrings(model.AllStringsOptions{OmitInteractiveBlocks: true})
		full := postFromJSON(c.post).AllStrings(model.AllStringsOptions{OmitInteractiveBlocks: false})
		res = append(res, postAllStringsCase{
			Name:     c.name,
			Post:     c.post,
			Omitting: omitting,
			Full:     full,
			Differs:  !stringSlicesEqual(omitting, full),
		})
	}
	return res
}

func stringSlicesEqual(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}
