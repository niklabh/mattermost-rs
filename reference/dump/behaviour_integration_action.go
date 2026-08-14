package main

// Behavioural oracle for model/integration_action.go — chunk 1, written to
// fixtures/behaviour_integration_action.json.
//
// integration_action.go is 1,390 lines. This chunk is `PostAction` and its immediate
// satellites, which is exactly what `MessageAttachment.Actions` needs; the Dialog family, the
// ECDSA trigger-id signing and the AES cookie encryption are later chunks.
//
// What this corpus exists to pin:
//
//  1. **`IsValid` returns an accumulated `*multierror.Error`, not the first failure.** Every
//     other `IsValid` ported so far returns a single `*AppError` and stops. Here the count and
//     the ORDER of the messages are the observable, and `Error()` has a specific layout that
//     changes between one error and several. Both are recorded verbatim.
//
//  2. **`Equals` ignores three fields it never compares** — Tooltip, Disabled and Style. Two
//     actions differing only in Style are "equal". Almost certainly fields added to the struct
//     without updating Equals, the same shape as PostMetadata.Copy ([D-034]).
//
//  3. **`Equals` dereferences option elements without a nil check**, so a nil option panics
//     where `IsValid` reports it politely. Probed under recover.
//
//  4. **The integration URL accepts a plugin-relative path**, so it is not simply
//     IsValidHTTPURL — `/plugins/x` and `plugins/x` pass, but `./plugins/x` does not.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/hashicorp/go-multierror"
	"github.com/mattermost/mattermost/server/public/model"
)

func writeIntegrationActionBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":                 integrationActionConstants(),
		"multierror_format":         multierrorFormatAll(),
		"post_action_is_valid":      postActionIsValidAll(),
		"post_action_options_valid": postActionOptionsIsValidAll(),
		"post_action_equals":        postActionEqualsAll(),
		"post_action_equals_panics": postActionEqualsPanics(),
		"normalize_format":          normalizeFormatAll(),
		"preserve_state":            postActionPreserveStateAll(),
		"wire":                      integrationActionWireAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_integration_action.json"), append(blob, '\n'), 0o644)
}

func integrationActionConstants() map[string]any {
	return map[string]any{
		"post_action_type_button":       model.PostActionTypeButton,
		"post_action_type_select":       model.PostActionTypeSelect,
		"post_action_data_source_users": model.PostActionDataSourceUsers,
		"post_action_data_source_chans": model.PostActionDataSourceChannels,

		"max_mm_blocks_actions_per_post": model.MaxMmBlocksActionsPerPost,
		"max_mm_blocks_action_key_len":   model.MaxMmBlocksActionKeyLength,
		"max_action_query_entries":       model.MaxActionQueryEntries,
		"max_action_query_key_length":    model.MaxActionQueryKeyLength,
		"max_action_query_value_length":  model.MaxActionQueryValueLength,

		"format_attachment":   model.PostActionIntegrationFormatAttachment,
		"format_apps_binding": model.PostActionIntegrationFormatAppsBinding,
		"format_block":        model.PostActionIntegrationFormatBlock,
		"format_card":         model.PostActionIntegrationFormatCard,
		"format_mm_block":     model.PostActionIntegrationFormatMmBlock,

		"mm_blocks_action_cookie_kind": model.MmBlocksActionCookieKind,
		"post_action_retain_prop_keys": model.PostActionRetainPropKeys,
	}
}

// --- the multierror layout ------------------------------------------------------------------

// The `Error()` string of a *multierror.Error is what a caller surfaces, and it is NOT a simple
// join: the one-error case has its own wording. Recorded rather than assumed, because the Rust
// port has to reproduce a format nobody would guess.
func multierrorFormatAll() []map[string]any {
	build := func(msgs ...string) *multierror.Error {
		var m *multierror.Error
		for _, s := range msgs {
			m = multierror.Append(m, staticErr(s))
		}
		return m
	}

	cases := []struct {
		name string
		msgs []string
	}{
		{"one", []string{"first"}},
		{"two", []string{"first", "second"}},
		{"three", []string{"a", "b", "c"}},
		{"empty_message", []string{""}},
		{"with_newline", []string{"first\nsecond"}},
	}

	res := make([]map[string]any, 0, len(cases)+1)
	for _, c := range cases {
		m := build(c.msgs...)
		res = append(res, map[string]any{
			"name":   c.name,
			"inputs": c.msgs,
			"error":  m.Error(),
			"count":  len(m.Errors),
		})
	}

	// multierror.Prefix, which PostAction.IsValid uses for nested option failures.
	inner := build("text is required", "value is required")
	prefixed := multierror.Prefix(inner, "option at index 0 is invalid:")
	row := map[string]any{"name": "prefixed", "error": prefixed.Error()}
	if pm, ok := prefixed.(*multierror.Error); ok {
		msgs := make([]string, 0, len(pm.Errors))
		for _, e := range pm.Errors {
			msgs = append(msgs, e.Error())
		}
		row["messages"] = msgs
		row["count"] = len(pm.Errors)
	}
	res = append(res, row)

	return res
}

type staticErr string

func (e staticErr) Error() string { return string(e) }

// --- PostAction.IsValid ----------------------------------------------------------------------

type postActionValidCase struct {
	Name   string          `json:"name"`
	Action json.RawMessage `json:"action"`
	// Every accumulated message, in order. The order is the order of the checks in IsValid,
	// not anything sorted.
	Messages []string `json:"messages"`
	Error    string   `json:"error"`
}

func postActionIsValidAll() []postActionValidCase {
	integration := func(u string) *model.PostActionIntegration {
		return &model.PostActionIntegration{URL: u}
	}
	opts := func(pairs ...string) []*model.PostActionOptions {
		var out []*model.PostActionOptions
		for i := 0; i+1 < len(pairs); i += 2 {
			out = append(out, &model.PostActionOptions{Text: pairs[i], Value: pairs[i+1]})
		}
		return out
	}

	cases := []struct {
		name string
		a    model.PostAction
	}{
		{"zero", model.PostAction{}},
		{"valid_button", model.PostAction{
			Name: "Go", Type: model.PostActionTypeButton,
			Integration: integration("https://example.com/hook"),
		}},
		{"valid_select_options", model.PostAction{
			Name: "Pick", Type: model.PostActionTypeSelect,
			Options:     opts("a", "1"),
			Integration: integration("https://example.com/hook"),
		}},
		{"valid_select_data_source", model.PostAction{
			Name: "Pick", Type: model.PostActionTypeSelect,
			DataSource:  model.PostActionDataSourceUsers,
			Integration: integration("https://example.com/hook"),
		}},

		// Name.
		{"no_name", model.PostAction{
			Type: model.PostActionTypeButton, Integration: integration("https://x.com"),
		}},

		// Style: six words or a SIX-digit hex colour. Note this regex is stricter than
		// channel.go's, which also takes three digits.
		{"style_default", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Style: "default",
			Integration: integration("https://x.com"),
		}},
		{"style_danger", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Style: "danger",
			Integration: integration("https://x.com"),
		}},
		{"style_hex6", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Style: "#a1b2c3",
			Integration: integration("https://x.com"),
		}},
		{"style_hex3", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Style: "#abc",
			Integration: integration("https://x.com"),
		}},
		{"style_hex_no_hash", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Style: "a1b2c3",
			Integration: integration("https://x.com"),
		}},
		{"style_hex_uppercase", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Style: "#A1B2C3",
			Integration: integration("https://x.com"),
		}},
		{"style_unknown_word", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Style: "Danger",
			Integration: integration("https://x.com"),
		}},

		// Type.
		{"type_empty", model.PostAction{Name: "n", Integration: integration("https://x.com")}},
		{"type_unknown", model.PostAction{
			Name: "n", Type: "radio", Integration: integration("https://x.com"),
		}},
		{"type_button_uppercase", model.PostAction{
			Name: "n", Type: "Button", Integration: integration("https://x.com"),
		}},
		{"button_with_options", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Options: opts("a", "1"),
			Integration: integration("https://x.com"),
		}},
		{"button_with_data_source", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, DataSource: model.PostActionDataSourceUsers,
			Integration: integration("https://x.com"),
		}},
		{"button_with_both", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Options: opts("a", "1"),
			DataSource: model.PostActionDataSourceUsers, Integration: integration("https://x.com"),
		}},

		// Select.
		{"select_no_options_no_source", model.PostAction{
			Name: "n", Type: model.PostActionTypeSelect, Integration: integration("https://x.com"),
		}},
		{"select_bad_data_source", model.PostAction{
			Name: "n", Type: model.PostActionTypeSelect, DataSource: "teams",
			Integration: integration("https://x.com"),
		}},
		{"select_source_and_options", model.PostAction{
			Name: "n", Type: model.PostActionTypeSelect, DataSource: model.PostActionDataSourceChannels,
			Options: opts("a", "1"), Integration: integration("https://x.com"),
		}},
		{"select_bad_source_and_options", model.PostAction{
			Name: "n", Type: model.PostActionTypeSelect, DataSource: "teams",
			Options: opts("a", "1"), Integration: integration("https://x.com"),
		}},
		{"select_empty_options_slice", model.PostAction{
			Name: "n", Type: model.PostActionTypeSelect, Options: []*model.PostActionOptions{},
			Integration: integration("https://x.com"),
		}},
		{"select_option_missing_text", model.PostAction{
			Name: "n", Type: model.PostActionTypeSelect, Options: opts("", "1"),
			Integration: integration("https://x.com"),
		}},
		{"select_option_missing_both", model.PostAction{
			Name: "n", Type: model.PostActionTypeSelect, Options: opts("", ""),
			Integration: integration("https://x.com"),
		}},
		{"select_second_option_bad", model.PostAction{
			Name: "n", Type: model.PostActionTypeSelect, Options: opts("a", "1", "", ""),
			Integration: integration("https://x.com"),
		}},
		{"select_nil_option", model.PostAction{
			Name: "n", Type: model.PostActionTypeSelect,
			Options:     []*model.PostActionOptions{nil},
			Integration: integration("https://x.com"),
		}},
		{"select_nil_then_bad_option", model.PostAction{
			Name: "n", Type: model.PostActionTypeSelect,
			Options:     []*model.PostActionOptions{nil, {Text: "", Value: ""}},
			Integration: integration("https://x.com"),
		}},

		// Integration URL: a plugin-relative path is accepted alongside a real HTTP URL.
		{"integration_nil", model.PostAction{Name: "n", Type: model.PostActionTypeButton}},
		{"integration_empty_url", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Integration: integration(""),
		}},
		{"integration_plugin_absolute", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Integration: integration("/plugins/x"),
		}},
		{"integration_plugin_relative", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Integration: integration("plugins/x"),
		}},
		{"integration_plugin_dot_relative", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Integration: integration("./plugins/x"),
		}},
		{"integration_plugin_prefix_only", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Integration: integration("/plugins/"),
		}},
		{"integration_plugins_lookalike", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Integration: integration("/pluginsx"),
		}},
		{"integration_http", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Integration: integration("http://x.com"),
		}},
		{"integration_ftp", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Integration: integration("ftp://x.com"),
		}},
		{"integration_relative_path", model.PostAction{
			Name: "n", Type: model.PostActionTypeButton, Integration: integration("/api/v4/x"),
		}},

		// Several failures at once, to pin the accumulation order.
		{"everything_wrong", model.PostAction{Style: "nope", Type: "nope"}},
		{"name_and_style_and_type", model.PostAction{
			Style: "nope", Type: "nope", Integration: integration("https://x.com"),
		}},
	}

	res := make([]postActionValidCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(&c.a)
		if err != nil {
			panic(err)
		}
		row := postActionValidCase{Name: c.name, Action: blob, Messages: []string{}}
		a := c.a
		if err := a.IsValid(); err != nil {
			row.Error = err.Error()
			if m, ok := err.(*multierror.Error); ok {
				for _, e := range m.Errors {
					row.Messages = append(row.Messages, e.Error())
				}
			}
		}
		res = append(res, row)
	}
	return res
}

func postActionOptionsIsValidAll() []map[string]any {
	cases := []struct {
		name string
		o    model.PostActionOptions
	}{
		{"both_set", model.PostActionOptions{Text: "t", Value: "v"}},
		{"no_text", model.PostActionOptions{Value: "v"}},
		{"no_value", model.PostActionOptions{Text: "t"}},
		{"neither", model.PostActionOptions{}},
		{"whitespace_text_counts_as_set", model.PostActionOptions{Text: " ", Value: "v"}},
	}
	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		o := c.o
		row := map[string]any{"name": c.name, "messages": []string{}, "error": ""}
		if err := o.IsValid(); err != nil {
			row["error"] = err.Error()
			if m, ok := err.(*multierror.Error); ok {
				msgs := make([]string, 0, len(m.Errors))
				for _, e := range m.Errors {
					msgs = append(msgs, e.Error())
				}
				row["messages"] = msgs
			}
		}
		res = append(res, row)
	}
	return res
}

// --- PostAction.Equals -------------------------------------------------------------------------

type postActionEqualsCase struct {
	Name   string          `json:"name"`
	A      json.RawMessage `json:"a"`
	B      json.RawMessage `json:"b"`
	Equals bool            `json:"equals"`
}

func postActionEqualsAll() []postActionEqualsCase {
	base := func() model.PostAction {
		return model.PostAction{
			Id: idA, Type: model.PostActionTypeSelect, Name: "Pick",
			Tooltip: "tip", Disabled: false, Style: "primary",
			DataSource: "", DefaultOption: "a", Cookie: "cookie",
			Options: []*model.PostActionOptions{{Text: "a", Value: "1"}},
			Integration: &model.PostActionIntegration{
				URL: "https://example.com/hook", Context: map[string]any{"k": "v"},
			},
		}
	}

	cases := []struct {
		name  string
		mutfn func(b *model.PostAction)
	}{
		{"identical", func(b *model.PostAction) {}},

		// The three fields Equals never looks at.
		{"tooltip_differs_still_equal", func(b *model.PostAction) { b.Tooltip = "other" }},
		{"disabled_differs_still_equal", func(b *model.PostAction) { b.Disabled = true }},
		{"style_differs_still_equal", func(b *model.PostAction) { b.Style = "danger" }},

		// The fields it does look at.
		{"id_differs", func(b *model.PostAction) { b.Id = idB }},
		{"type_differs", func(b *model.PostAction) { b.Type = model.PostActionTypeButton }},
		{"name_differs", func(b *model.PostAction) { b.Name = "Other" }},
		{"data_source_differs", func(b *model.PostAction) { b.DataSource = "users" }},
		{"default_option_differs", func(b *model.PostAction) { b.DefaultOption = "z" }},
		{"cookie_differs", func(b *model.PostAction) { b.Cookie = "other" }},

		{"option_text_differs", func(b *model.PostAction) {
			b.Options = []*model.PostActionOptions{{Text: "z", Value: "1"}}
		}},
		{"option_value_differs", func(b *model.PostAction) {
			b.Options = []*model.PostActionOptions{{Text: "a", Value: "9"}}
		}},
		{"option_count_differs", func(b *model.PostAction) {
			b.Options = []*model.PostActionOptions{{Text: "a", Value: "1"}, {Text: "b", Value: "2"}}
		}},
		{"options_emptied", func(b *model.PostAction) { b.Options = nil }},

		{"integration_url_differs", func(b *model.PostAction) {
			b.Integration = &model.PostActionIntegration{
				URL: "https://other.com", Context: map[string]any{"k": "v"},
			}
		}},
		{"integration_nil_on_b", func(b *model.PostAction) { b.Integration = nil }},
		{"context_value_differs", func(b *model.PostAction) {
			b.Integration = &model.PostActionIntegration{
				URL: "https://example.com/hook", Context: map[string]any{"k": "other"},
			}
		}},
		{"context_key_differs", func(b *model.PostAction) {
			b.Integration = &model.PostActionIntegration{
				URL: "https://example.com/hook", Context: map[string]any{"other": "v"},
			}
		}},
		{"context_count_differs", func(b *model.PostAction) {
			b.Integration = &model.PostActionIntegration{
				URL: "https://example.com/hook", Context: map[string]any{"k": "v", "j": "w"},
			}
		}},
		{"context_nested_equal", func(b *model.PostAction) {
			b.Integration = &model.PostActionIntegration{
				URL: "https://example.com/hook", Context: map[string]any{"k": "v"},
			}
		}},
	}

	res := make([]postActionEqualsCase, 0, len(cases)+2)
	for _, c := range cases {
		a := base()
		b := base()
		c.mutfn(&b)
		ab, _ := json.Marshal(&a)
		bb, _ := json.Marshal(&b)
		res = append(res, postActionEqualsCase{
			Name: c.name, A: ab, B: bb, Equals: a.Equals(&b),
		})
	}

	// Both integrations nil is equal; a nested map compares by DeepEqual.
	{
		a, b := base(), base()
		a.Integration, b.Integration = nil, nil
		ab, _ := json.Marshal(&a)
		bb, _ := json.Marshal(&b)
		res = append(res, postActionEqualsCase{
			Name: "both_integrations_nil", A: ab, B: bb, Equals: a.Equals(&b),
		})
	}
	{
		a, b := base(), base()
		nested := func() map[string]any {
			return map[string]any{"k": map[string]any{"deep": []any{"x", float64(1)}}}
		}
		a.Integration = &model.PostActionIntegration{URL: "u", Context: nested()}
		b.Integration = &model.PostActionIntegration{URL: "u", Context: nested()}
		ab, _ := json.Marshal(&a)
		bb, _ := json.Marshal(&b)
		res = append(res, postActionEqualsCase{
			Name: "nested_context_deep_equal", A: ab, B: bb, Equals: a.Equals(&b),
		})
	}

	return res
}

// Equals indexes p.Options[k].Text with no nil check, so a nil element panics where IsValid
// reports it. Measured rather than reasoned about — the DelProp probe in behaviour_post.go
// predicted a panic that never came, so predictions here are worth nothing.
func postActionEqualsPanics() map[string]any {
	probe := func(a, b *model.PostAction) (panicked bool) {
		defer func() {
			if r := recover(); r != nil {
				panicked = true
			}
		}()
		a.Equals(b)
		return false
	}

	withNil := &model.PostAction{Options: []*model.PostActionOptions{nil}}
	withReal := &model.PostAction{Options: []*model.PostActionOptions{{Text: "a", Value: "1"}}}
	empty := &model.PostAction{}

	return map[string]any{
		"nil_option_on_receiver": probe(withNil, withReal),
		"nil_option_on_input":    probe(withReal, withNil),
		"nil_option_on_both":     probe(withNil, withNil),
		"no_options":             probe(empty, empty),
	}
}

// --- NormalizePostActionIntegrationFormat -------------------------------------------------------

func normalizeFormatAll() map[string]string {
	inputs := []string{
		"", "attachment", "apps_binding", "block", "card", "mm_block",
		// TrimSpace then ToLower, so padding and casing are both absorbed.
		" mm_block ", "MM_BLOCK", "Mm_Block", "\tcard\n", "  ",
		"unknown", "attachments", "mm_blocks", "BLOCK ", "mm block",
	}
	res := make(map[string]string, len(inputs))
	for _, in := range inputs {
		res[in] = model.NormalizePostActionIntegrationFormat(in)
	}
	return res
}

// --- Post.PostActionPreserveState ----------------------------------------------------------------

type preserveStateCase struct {
	Name        string          `json:"name"`
	Props       json.RawMessage `json:"props"`
	PostID      string          `json:"post_id"`
	RootID      string          `json:"root_id"`
	Retain      json.RawMessage `json:"retain"`
	Remove      []string        `json:"remove"`
	OrigProps   json.RawMessage `json:"original_props"`
	RootPostID  string          `json:"root_post_id"`
	IsPinned    bool            `json:"original_is_pinned"`
	HasReaction bool            `json:"original_has_reactions"`
}

func postActionPreserveStateAll() []preserveStateCase {
	cases := []struct {
		name       string
		props      model.StringInterface
		id, rootID string
		pinned     bool
		reactions  bool
	}{
		{"nil_props", nil, idA, "", false, false},
		{"empty_props", model.StringInterface{}, idA, "", false, false},
		{"all_five_present", model.StringInterface{
			model.PostPropsFromWebhook:      "true",
			model.PostPropsFromBot:          "true",
			model.PostPropsFromPlugin:       "true",
			model.PostPropsOverrideUsername: "bot",
			model.PostPropsOverrideIconURL:  "https://x.com/i.png",
		}, idA, "", false, false},
		{"some_present", model.StringInterface{
			model.PostPropsFromWebhook: "true",
			"unrelated":                "kept_in_original_only",
		}, idA, "", false, false},
		// A stored null is PRESENT, so it is retained rather than removed.
		{"null_value_is_retained", model.StringInterface{
			model.PostPropsFromBot: nil,
		}, idA, "", false, false},
		{"root_id_wins", model.StringInterface{}, idA, idB, false, false},
		{"pinned_and_reactions", model.StringInterface{}, idA, "", true, true},
	}

	res := make([]preserveStateCase, 0, len(cases))
	for _, c := range cases {
		p := &model.Post{
			Id: c.id, RootId: c.rootID, Props: c.props,
			IsPinned: c.pinned, HasReactions: c.reactions,
		}
		st := p.PostActionPreserveState()

		propsBlob, _ := json.Marshal(c.props)
		retainBlob, _ := json.Marshal(st.Retain)
		origBlob, _ := json.Marshal(st.OriginalProps)
		res = append(res, preserveStateCase{
			Name: c.name, Props: propsBlob, PostID: c.id, RootID: c.rootID,
			Retain: retainBlob, Remove: st.Remove, OrigProps: origBlob,
			RootPostID: st.RootPostId, IsPinned: st.OriginalIsPinned,
			HasReaction: st.OriginalHasReactions,
		})
	}
	return res
}

// --- wire ----------------------------------------------------------------------------------------

type iaWireCase struct {
	Name string `json:"name"`
	JSON string `json:"json"`
}

func integrationActionWireAll() []iaWireCase {
	cases := []struct {
		name string
		v    any
	}{
		{"post_action_zero", &model.PostAction{}},
		{"post_action_disabled_false_is_omitted", &model.PostAction{Name: "n", Disabled: false}},
		{"post_action_disabled_true", &model.PostAction{Name: "n", Disabled: true}},
		{"post_action_empty_options_slice", &model.PostAction{
			Options: []*model.PostActionOptions{},
		}},
		{"post_action_full", &model.PostAction{
			Id: idA, Type: model.PostActionTypeSelect, Name: "Pick", Tooltip: "tip",
			Disabled: true, Style: "#a1b2c3", DataSource: model.PostActionDataSourceUsers,
			Options:       []*model.PostActionOptions{{Text: "t", Value: "v"}},
			DefaultOption: "v",
			Integration: &model.PostActionIntegration{
				URL: "https://example.com/h", Context: map[string]any{"b": 2, "a": "1"},
			},
			Cookie: "ck",
		}},
		// PostActionOptions has NO omitempty on either field.
		{"post_action_options_zero", &model.PostActionOptions{}},
		{"post_action_integration_zero", &model.PostActionIntegration{}},
		{"post_action_integration_empty_context", &model.PostActionIntegration{
			URL: "u", Context: map[string]any{},
		}},
		{"do_post_action_request_zero", &model.DoPostActionRequest{}},
		{"do_post_action_request_full", &model.DoPostActionRequest{
			SelectedOption: "o", Cookie: "c",
			Query:             map[string]string{"k": "v"},
			IntegrationFormat: model.PostActionIntegrationFormatMmBlock,
		}},
		{"post_action_cookie_zero", &model.PostActionCookie{}},
		{"post_action_cookie_full", &model.PostActionCookie{
			Type: "t", PostId: idA, RootPostId: idB, ChannelId: idC, DataSource: "users",
			Integration: &model.PostActionIntegration{URL: "u"},
			RetainProps: map[string]any{"a": "b"}, RemoveProps: []string{"c"},
		}},
		// Actions has NO omitempty, so a nil map is `null` and the key is always present.
		{"mm_blocks_cookie_zero", &model.MmBlocksActionCookie{}},
		{"mm_blocks_cookie_full", &model.MmBlocksActionCookie{
			Kind: model.MmBlocksActionCookieKind, PostId: idA, RootPostId: idB, ChannelId: idC,
			RetainProps: map[string]any{"a": "b"}, RemoveProps: []string{"c"},
			Actions: map[string]map[string]any{"k": {"x": "y"}},
		}},
		{"integration_request_zero", &model.PostActionIntegrationRequest{}},
		{"integration_request_full", &model.PostActionIntegrationRequest{
			UserId: idA, UserName: "u", ChannelId: idB, ChannelName: "c",
			TeamId: idC, TeamName: "t", PostId: idA, TriggerId: "tr",
			Type: "button", DataSource: "users", Context: map[string]any{"a": "b"},
		}},
		// Update is a *Post with no omitempty: nil is `null`.
		{"integration_response_zero", &model.PostActionIntegrationResponse{}},
		{"integration_response_full", &model.PostActionIntegrationResponse{
			Update: &model.Post{Id: idA}, EphemeralText: "e",
			SkipSlackParsing: true, GotoLocation: "g",
		}},
		{"api_response_zero", &model.PostActionAPIResponse{}},
		{"api_response_full", &model.PostActionAPIResponse{
			Status: "OK", TriggerId: "tr", GotoLocation: "g",
		}},
		{"execute_dialog_response_zero", &model.ExecuteDialogActionResponse{}},
	}

	res := make([]iaWireCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(c.v)
		if err != nil {
			panic(err)
		}
		res = append(res, iaWireCase{Name: c.name, JSON: string(blob)})
	}
	return res
}
