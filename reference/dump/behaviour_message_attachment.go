package main

// Behavioural oracle for model/message_attachment.go, written to
// fixtures/behaviour_message_attachment.json.
//
// The whole file, 292 lines. The traps it is built around:
//
//  1. **`Timestamp` and `Field.Value` are bare `any`, and their validators accept Go types that
//     JSON cannot produce.** `IsValid` takes `string` or `int64` for Timestamp and `string` or
//     `int` for Value — but `encoding/json` decodes every number into a **float64**. So an
//     attachment that arrives over the wire carrying `"ts": 123` is *invalid*, while the same
//     struct built in Go code with an int64 is valid. Recorded from both directions.
//
//  2. **`StringifyMessageAttachmentFieldValue` renders with `fmt.Sprintf("%v")`**, so the
//     output is Go's formatting, not JSON's: a float64 prints via `%g`, a map prints as
//     `map[k:v]` with sorted keys, a nil pointer as `<nil>`. None of that is reproducible by
//     guessing.
//
//  3. **The colour word list is NOT the action style list.** Attachments take
//     good/warning/danger; `PostAction.Style` takes six words. Both share the six-digit hex
//     regex.
//
//  4. **It mutates in place and returns a filtered copy.** Stringify drops nil attachments and
//     nil fields from the slices it returns while rewriting the elements it keeps.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"

	"github.com/hashicorp/go-multierror"
	"github.com/mattermost/mattermost/server/public/model"
)

func writeMessageAttachmentBehaviourFixture(outDir string) error {
	out := map[string]any{
		"wire":                maWireAll(),
		"is_valid":            maIsValidAll(),
		"field_is_valid":      maFieldIsValidAll(),
		"go_type_vs_json":     maGoTypeVsJSON(),
		"equals":              maEqualsAll(),
		"field_equals":        maFieldEqualsAll(),
		"stringify":           maStringifyAll(),
		"sprintf_v":           maSprintfAll(),
		"parse_slack_links":   parseSlackLinksAll(),
		"parse_attachment":    parseMessageAttachmentAll(),
		"equals_from_json":    maEqualsFromJSON(),
		"go_format_v_floats":  goFormatVFloats(),
		"sprintf_v_from_json": sprintfVFromJSON(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_message_attachment.json"), append(blob, '\n'), 0o644)
}

// --- wire ------------------------------------------------------------------------------------

type maWireCase struct {
	Name      string `json:"name"`
	JSON      string `json:"json"`
	Roundtrip string `json:"roundtrip"`
}

func maWireAll() []maWireCase {
	cases := []struct {
		name string
		v    any
	}{
		// Only Actions carries omitempty; every other key is always present.
		{"attachment_zero", &model.MessageAttachment{}},
		{"attachment_fields_empty_slice", &model.MessageAttachment{
			Fields: []*model.MessageAttachmentField{},
		}},
		{"attachment_actions_empty_slice", &model.MessageAttachment{
			Actions: []*model.PostAction{},
		}},
		{"attachment_ts_string", &model.MessageAttachment{Timestamp: "123"}},
		{"attachment_ts_int64", &model.MessageAttachment{Timestamp: int64(123)}},
		{"attachment_ts_float", &model.MessageAttachment{Timestamp: 1.5}},
		{"attachment_ts_bool", &model.MessageAttachment{Timestamp: true}},
		{"attachment_full", &model.MessageAttachment{
			Id: 7, Fallback: "fb", Color: "#a1b2c3", Pretext: "pre",
			AuthorName: "an", AuthorLink: "https://example.com/a",
			AuthorIcon: "https://example.com/i.png",
			Title:      "t", TitleLink: "https://example.com/t", Text: "txt",
			Fields: []*model.MessageAttachmentField{
				{Title: "ft", Value: "fv", Short: true},
			},
			ImageURL: "https://example.com/img.png",
			ThumbURL: "https://example.com/th.png",
			Footer:   "f", FooterIcon: "https://example.com/f.png",
			Timestamp: int64(1700000000),
			Actions: []*model.PostAction{
				{Name: "Go", Type: model.PostActionTypeButton},
			},
		}},

		// Field: no omitempty anywhere, and Short marshals as a plain bool.
		{"field_zero", &model.MessageAttachmentField{}},
		{"field_short_true", &model.MessageAttachmentField{Short: true}},
		{"field_value_string", &model.MessageAttachmentField{Value: "v"}},
		{"field_value_number", &model.MessageAttachmentField{Value: 3}},
		{"field_value_object", &model.MessageAttachmentField{
			Value: map[string]any{"b": 2, "a": 1},
		}},
		{"field_value_array", &model.MessageAttachmentField{Value: []any{"x", 1}}},
	}

	res := make([]maWireCase, 0, len(cases))
	for _, c := range cases {
		blob, err := json.Marshal(c.v)
		if err != nil {
			panic(err)
		}
		row := maWireCase{Name: c.name, JSON: string(blob)}

		// Round-trip through the matching type so the lossy `any` fields are visible.
		switch c.v.(type) {
		case *model.MessageAttachment:
			var back model.MessageAttachment
			if err := json.Unmarshal(blob, &back); err != nil {
				panic(err)
			}
			rt, _ := json.Marshal(&back)
			row.Roundtrip = string(rt)
		case *model.MessageAttachmentField:
			var back model.MessageAttachmentField
			if err := json.Unmarshal(blob, &back); err != nil {
				panic(err)
			}
			rt, _ := json.Marshal(&back)
			row.Roundtrip = string(rt)
		}
		res = append(res, row)
	}
	return res
}

// --- IsValid ---------------------------------------------------------------------------------

type maValidCase struct {
	Name       string          `json:"name"`
	Attachment json.RawMessage `json:"attachment"`
	// Timestamp and Field.Value are `any`; the marshalled form cannot say what Go TYPE they
	// held, and the type is exactly what IsValid checks. So the Rust side rebuilds these
	// from `name` rather than from the JSON — see go_type_vs_json.
	Messages []string `json:"messages"`
	Error    string   `json:"error"`
}

func maIsValidAll() []maValidCase {
	base := func() model.MessageAttachment { return model.MessageAttachment{} }

	cases := []struct {
		name string
		mut  func(a *model.MessageAttachment)
	}{
		{"zero", func(a *model.MessageAttachment) {}},

		// Color: three words, or a SIX-digit hex. Not the six-word PostAction.Style list.
		{"color_good", func(a *model.MessageAttachment) { a.Color = "good" }},
		{"color_warning", func(a *model.MessageAttachment) { a.Color = "warning" }},
		{"color_danger", func(a *model.MessageAttachment) { a.Color = "danger" }},
		{"color_primary_is_not_a_colour", func(a *model.MessageAttachment) { a.Color = "primary" }},
		{"color_default_is_not_a_colour", func(a *model.MessageAttachment) { a.Color = "default" }},
		{"color_hex6", func(a *model.MessageAttachment) { a.Color = "#a1b2c3" }},
		{"color_hex6_upper", func(a *model.MessageAttachment) { a.Color = "#A1B2C3" }},
		{"color_hex3", func(a *model.MessageAttachment) { a.Color = "#abc" }},
		{"color_hex_no_hash", func(a *model.MessageAttachment) { a.Color = "a1b2c3" }},
		{"color_Good_wrong_case", func(a *model.MessageAttachment) { a.Color = "Good" }},

		// AuthorLink needs AuthorName AND a valid URL — two independent checks.
		{"author_link_without_name", func(a *model.MessageAttachment) {
			a.AuthorLink = "https://example.com/a"
		}},
		{"author_link_with_name", func(a *model.MessageAttachment) {
			a.AuthorName = "an"
			a.AuthorLink = "https://example.com/a"
		}},
		{"author_link_bad_url_with_name", func(a *model.MessageAttachment) {
			a.AuthorName = "an"
			a.AuthorLink = "notaurl"
		}},
		{"author_link_bad_url_without_name", func(a *model.MessageAttachment) {
			a.AuthorLink = "notaurl"
		}},
		{"author_link_plugin_path_is_not_accepted", func(a *model.MessageAttachment) {
			a.AuthorName = "an"
			a.AuthorLink = "/plugins/x"
		}},

		{"author_icon_ok", func(a *model.MessageAttachment) {
			a.AuthorIcon = "https://example.com/i.png"
		}},
		{"author_icon_bad", func(a *model.MessageAttachment) { a.AuthorIcon = "notaurl" }},

		// TitleLink mirrors AuthorLink.
		{"title_link_without_title", func(a *model.MessageAttachment) {
			a.TitleLink = "https://example.com/t"
		}},
		{"title_link_with_title", func(a *model.MessageAttachment) {
			a.Title = "t"
			a.TitleLink = "https://example.com/t"
		}},
		{"title_link_bad_url_without_title", func(a *model.MessageAttachment) {
			a.TitleLink = "notaurl"
		}},

		{"image_url_bad", func(a *model.MessageAttachment) { a.ImageURL = "notaurl" }},
		{"thumb_url_bad", func(a *model.MessageAttachment) { a.ThumbURL = "notaurl" }},
		{"footer_icon_bad", func(a *model.MessageAttachment) { a.FooterIcon = "notaurl" }},
		{"all_urls_bad", func(a *model.MessageAttachment) {
			a.ImageURL = "x"
			a.ThumbURL = "y"
			a.FooterIcon = "z"
		}},

		// Fields. A nil element is dereferenced by IsValid -> measured separately.
		{"field_value_string", func(a *model.MessageAttachment) {
			a.Fields = []*model.MessageAttachmentField{{Title: "t", Value: "v"}}
		}},
		{"field_value_int", func(a *model.MessageAttachment) {
			a.Fields = []*model.MessageAttachmentField{{Title: "t", Value: 3}}
		}},
		{"field_value_int64_is_invalid", func(a *model.MessageAttachment) {
			a.Fields = []*model.MessageAttachmentField{{Title: "t", Value: int64(3)}}
		}},
		{"field_value_float_is_invalid", func(a *model.MessageAttachment) {
			a.Fields = []*model.MessageAttachmentField{{Title: "t", Value: 1.5}}
		}},
		{"field_value_bool_is_invalid", func(a *model.MessageAttachment) {
			a.Fields = []*model.MessageAttachmentField{{Title: "t", Value: true}}
		}},
		{"field_value_nil_is_fine", func(a *model.MessageAttachment) {
			a.Fields = []*model.MessageAttachmentField{{Title: "t", Value: nil}}
		}},
		{"two_bad_fields", func(a *model.MessageAttachment) {
			a.Fields = []*model.MessageAttachmentField{
				{Title: "a", Value: true}, {Title: "b", Value: 1.5},
			}
		}},

		// Timestamp: string or int64 only.
		{"ts_string", func(a *model.MessageAttachment) { a.Timestamp = "123" }},
		{"ts_int64", func(a *model.MessageAttachment) { a.Timestamp = int64(123) }},
		{"ts_int_is_invalid", func(a *model.MessageAttachment) { a.Timestamp = 123 }},
		{"ts_float_is_invalid", func(a *model.MessageAttachment) { a.Timestamp = 1.5 }},
		{"ts_bool_is_invalid", func(a *model.MessageAttachment) { a.Timestamp = true }},
		{"ts_nil_is_fine", func(a *model.MessageAttachment) { a.Timestamp = nil }},

		// Actions are validated with a positional prefix.
		{"action_invalid", func(a *model.MessageAttachment) {
			a.Actions = []*model.PostAction{{}}
		}},
		{"action_valid", func(a *model.MessageAttachment) {
			a.Actions = []*model.PostAction{{
				Name: "Go", Type: model.PostActionTypeButton,
				Integration: &model.PostActionIntegration{URL: "https://example.com/h"},
			}}
		}},
		{"second_action_invalid", func(a *model.MessageAttachment) {
			a.Actions = []*model.PostAction{
				{
					Name: "Go", Type: model.PostActionTypeButton,
					Integration: &model.PostActionIntegration{URL: "https://example.com/h"},
				},
				{},
			}
		}},

		// Everything at once, to pin the accumulation order across sections.
		{"many_failures", func(a *model.MessageAttachment) {
			a.Color = "nope"
			a.AuthorLink = "bad"
			a.TitleLink = "bad"
			a.ImageURL = "bad"
			a.ThumbURL = "bad"
			a.FooterIcon = "bad"
			a.Timestamp = 1.5
			a.Fields = []*model.MessageAttachmentField{{Value: true}}
			a.Actions = []*model.PostAction{{}}
		}},
	}

	res := make([]maValidCase, 0, len(cases))
	for _, c := range cases {
		a := base()
		c.mut(&a)
		blob, err := json.Marshal(&a)
		if err != nil {
			panic(err)
		}
		row := maValidCase{Name: c.name, Attachment: blob, Messages: []string{}}
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

func maFieldIsValidAll() []map[string]any {
	cases := []struct {
		name string
		v    any
	}{
		{"nil", nil},
		{"string", "x"},
		{"empty_string", ""},
		{"int", 3},
		{"int64", int64(3)},
		{"float64", 1.5},
		{"float64_whole", 1.0},
		{"bool", true},
		{"slice", []any{1}},
		{"map", map[string]any{"a": 1}},
	}
	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		f := model.MessageAttachmentField{Title: "t", Value: c.v}
		row := map[string]any{"name": c.name, "messages": []string{}}
		if err := f.IsValid(); err != nil {
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

// The headline trap, isolated: the same JSON validates differently depending on whether the
// struct was built in Go or decoded from the wire, because encoding/json makes every number a
// float64 and neither validator accepts float64.
func maGoTypeVsJSON() []map[string]any {
	blobs := []string{
		`{"ts":123}`,
		`{"ts":1.5}`,
		`{"ts":"123"}`,
		`{"ts":null}`,
		`{"fields":[{"title":"t","value":123}]}`,
		`{"fields":[{"title":"t","value":"v"}]}`,
		`{"fields":[{"title":"t","value":null}]}`,
		`{"fields":[{"title":"t","value":1.5}]}`,
	}
	res := make([]map[string]any, 0, len(blobs))
	for _, blob := range blobs {
		var a model.MessageAttachment
		if err := json.Unmarshal([]byte(blob), &a); err != nil {
			panic(err)
		}
		row := map[string]any{"json": blob, "messages": []string{}}
		if err := a.IsValid(); err != nil {
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

// --- Equals ------------------------------------------------------------------------------------

type maEqualsCase struct {
	Name   string `json:"name"`
	Equals bool   `json:"equals"`
}

func maEqualsAll() []maEqualsCase {
	base := func() model.MessageAttachment {
		return model.MessageAttachment{
			Id: 1, Fallback: "fb", Color: "good", Pretext: "pre",
			AuthorName: "an", AuthorLink: "al", AuthorIcon: "ai",
			Title: "t", TitleLink: "tl", Text: "tx",
			Fields:   []*model.MessageAttachmentField{{Title: "ft", Value: "fv", Short: true}},
			ImageURL: "iu", ThumbURL: "tu", Footer: "f", FooterIcon: "fi",
			Timestamp: int64(1),
			Actions:   []*model.PostAction{{Id: idA, Name: "n"}},
		}
	}

	cases := []struct {
		name string
		mut  func(b *model.MessageAttachment)
	}{
		{"identical", func(b *model.MessageAttachment) {}},
		{"id", func(b *model.MessageAttachment) { b.Id = 2 }},
		{"fallback", func(b *model.MessageAttachment) { b.Fallback = "x" }},
		{"color", func(b *model.MessageAttachment) { b.Color = "danger" }},
		{"pretext", func(b *model.MessageAttachment) { b.Pretext = "x" }},
		{"author_name", func(b *model.MessageAttachment) { b.AuthorName = "x" }},
		{"author_link", func(b *model.MessageAttachment) { b.AuthorLink = "x" }},
		{"author_icon", func(b *model.MessageAttachment) { b.AuthorIcon = "x" }},
		{"title", func(b *model.MessageAttachment) { b.Title = "x" }},
		{"title_link", func(b *model.MessageAttachment) { b.TitleLink = "x" }},
		{"text", func(b *model.MessageAttachment) { b.Text = "x" }},
		{"image_url", func(b *model.MessageAttachment) { b.ImageURL = "x" }},
		{"thumb_url", func(b *model.MessageAttachment) { b.ThumbURL = "x" }},
		{"footer", func(b *model.MessageAttachment) { b.Footer = "x" }},
		{"footer_icon", func(b *model.MessageAttachment) { b.FooterIcon = "x" }},

		{"field_count", func(b *model.MessageAttachment) { b.Fields = nil }},
		{"field_title", func(b *model.MessageAttachment) {
			b.Fields = []*model.MessageAttachmentField{{Title: "x", Value: "fv", Short: true}}
		}},
		{"field_value", func(b *model.MessageAttachment) {
			b.Fields = []*model.MessageAttachmentField{{Title: "ft", Value: "x", Short: true}}
		}},
		{"field_short", func(b *model.MessageAttachment) {
			b.Fields = []*model.MessageAttachmentField{{Title: "ft", Value: "fv", Short: false}}
		}},

		{"action_count", func(b *model.MessageAttachment) { b.Actions = nil }},
		{"action_id", func(b *model.MessageAttachment) {
			b.Actions = []*model.PostAction{{Id: idB, Name: "n"}}
		}},

		// Timestamp is compared with `==` on two `any`s, so the dynamic TYPE matters.
		{"ts_same", func(b *model.MessageAttachment) { b.Timestamp = int64(1) }},
		{"ts_different_value", func(b *model.MessageAttachment) { b.Timestamp = int64(2) }},
		{"ts_same_value_different_type", func(b *model.MessageAttachment) { b.Timestamp = 1 }},
		{"ts_string_vs_int", func(b *model.MessageAttachment) { b.Timestamp = "1" }},
		{"ts_nil", func(b *model.MessageAttachment) { b.Timestamp = nil }},
	}

	res := make([]maEqualsCase, 0, len(cases))
	for _, c := range cases {
		a := base()
		b := base()
		c.mut(&b)
		res = append(res, maEqualsCase{Name: c.name, Equals: a.Equals(&b)})
	}
	return res
}

func maFieldEqualsAll() []map[string]any {
	cases := []struct {
		name string
		a, b model.MessageAttachmentField
	}{
		{"identical", model.MessageAttachmentField{Title: "t", Value: "v"}, model.MessageAttachmentField{Title: "t", Value: "v"}},
		{"title_differs", model.MessageAttachmentField{Title: "t"}, model.MessageAttachmentField{Title: "x"}},
		// Value nil on either side PANICS, so Short can only be isolated with values set.
		{"short_differs_both_values_nil", model.MessageAttachmentField{Short: true}, model.MessageAttachmentField{Short: false}},
		{"short_differs_values_set", model.MessageAttachmentField{Value: "v", Short: true}, model.MessageAttachmentField{Value: "v", Short: false}},
		{"short_same_values_set", model.MessageAttachmentField{Value: "v", Short: true}, model.MessageAttachmentField{Value: "v", Short: true}},
		{"value_differs", model.MessageAttachmentField{Value: "a"}, model.MessageAttachmentField{Value: "b"}},
		{"value_same_number", model.MessageAttachmentField{Value: 1}, model.MessageAttachmentField{Value: 1}},
		{"value_int_vs_float", model.MessageAttachmentField{Value: 1}, model.MessageAttachmentField{Value: 1.0}},
		{"value_int_vs_string", model.MessageAttachmentField{Value: 1}, model.MessageAttachmentField{Value: "1"}},
		{"value_both_nil", model.MessageAttachmentField{}, model.MessageAttachmentField{}},
		{"value_nil_vs_set", model.MessageAttachmentField{}, model.MessageAttachmentField{Value: "a"}},
		{"value_maps_equal", model.MessageAttachmentField{Value: map[string]any{"a": 1}}, model.MessageAttachmentField{Value: map[string]any{"a": 1}}},
		{"value_maps_differ", model.MessageAttachmentField{Value: map[string]any{"a": 1}}, model.MessageAttachmentField{Value: map[string]any{"a": 2}}},
		{"value_slices_equal", model.MessageAttachmentField{Value: []any{1, "x"}}, model.MessageAttachmentField{Value: []any{1, "x"}}},
	}

	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		row := map[string]any{"name": c.name}
		func() {
			defer func() {
				if r := recover(); r != nil {
					row["panicked"] = true
				}
			}()
			a := c.a
			row["equals"] = a.Equals(&c.b)
			row["panicked"] = false
		}()
		res = append(res, row)
	}
	return res
}

// --- Stringify ------------------------------------------------------------------------------------

func maStringifyAll() []map[string]any {
	build := func() []*model.MessageAttachment {
		return []*model.MessageAttachment{
			nil,
			{
				Text: "keep",
				Fields: []*model.MessageAttachmentField{
					nil,
					{Title: "a", Value: "string"},
					{Title: "b", Value: 3},
					{Title: "c", Value: 1.5},
					{Title: "d", Value: true},
					{Title: "e", Value: nil},
					{Title: "f", Value: []any{1, "x"}},
					{Title: "g", Value: map[string]any{"z": 1, "a": 2}},
				},
			},
			nil,
			{Text: "second", Fields: nil},
		}
	}

	in := build()
	out := model.StringifyMessageAttachmentFieldValue(in)

	outBlob, _ := json.Marshal(out)
	return []map[string]any{{
		"in_len":  len(in),
		"out_len": len(out),
		"out":     json.RawMessage(outBlob),
	}}
}

// Go's fmt.Sprintf("%v") for the value kinds a decoded field can hold. This is Go's formatting,
// not JSON's — a float64 uses %g, a map prints with sorted keys inside `map[...]`.
func maSprintfAll() []map[string]any {
	values := []struct {
		name string
		v    any
	}{
		{"string", "x"},
		{"empty_string", ""},
		{"int", 3},
		{"float_whole", 1.0},
		{"float_frac", 1.5},
		{"float_big", 1e21},
		{"float_small", 0.000001},
		{"float_neg", -2.5},
		{"bool_true", true},
		{"bool_false", false},
		{"slice", []any{1, "x", true}},
		{"empty_slice", []any{}},
		{"map_sorted", map[string]any{"z": 1, "a": 2}},
		{"empty_map", map[string]any{}},
		{"nested", map[string]any{"a": []any{1, map[string]any{"b": 2}}}},
	}

	res := make([]map[string]any, 0, len(values))
	for _, v := range values {
		f := &model.MessageAttachmentField{Title: "t", Value: v.v}
		attachments := model.StringifyMessageAttachmentFieldValue(
			[]*model.MessageAttachment{{Fields: []*model.MessageAttachmentField{f}}},
		)
		out := attachments[0].Fields[0].Value
		row := map[string]any{"name": v.name}
		if s, ok := out.(string); ok {
			row["formatted"] = s
		} else {
			row["formatted"] = nil
		}
		res = append(res, row)
	}
	return res
}

// --- ParseSlackLinksToMarkdown ------------------------------------------------------------------

func parseSlackLinksAll() map[string]string {
	inputs := []string{
		"",
		"plain text",
		"<https://example.com|Example>",
		"before <https://example.com|Example> after",
		"<https://a.com|A> and <https://b.com|B>",
		// The URL part rejects `<` and `|`; the text part rejects `>`.
		"<https://example.com>",
		"<|Example>",
		"<https://example.com|>",
		"<|>",
		"<a|b|c>",
		"<a<b|c>",
		"<a|b>c>",
		"no closing <https://example.com|Example",
		"nested <<a|b>|c>",
		"<mailto:a@b.com|Mail>",
		"<#channel|Channel>",
		"<@user|User>",
		// Markdown-significant characters in the replacement text.
		"<https://example.com|[bracket]>",
		"<https://example.com/(paren)|Text>",
		"multi\nline <https://a.com|A>",
		"<a|b> <c|d> <e|f>",
	}
	res := make(map[string]string, len(inputs))
	for _, in := range inputs {
		res[in] = model.ParseSlackLinksToMarkdown(in)
	}
	return res
}

// --- ParseMessageAttachment ------------------------------------------------------------------------

func parseMessageAttachmentAll() []map[string]any {
	cases := []struct {
		name        string
		postType    string
		attachments []*model.MessageAttachment
	}{
		{"empty_type_becomes_slack_attachment", "", []*model.MessageAttachment{
			{Text: "<https://a.com|A>", Pretext: "<https://b.com|B>"},
		}},
		{"existing_type_is_kept", model.PostTypeDefault + "custom_x", []*model.MessageAttachment{
			{Text: "plain"},
		}},
		{"nil_attachments_are_dropped", "", []*model.MessageAttachment{
			nil, {Text: "kept"}, nil,
		}},
		{"nil_fields_are_kept_but_skipped", "", []*model.MessageAttachment{
			{Fields: []*model.MessageAttachmentField{
				nil,
				{Title: "a", Value: "<https://a.com|A>"},
				{Title: "b", Value: 3},
			}},
		}},
		{"no_attachments", "", []*model.MessageAttachment{}},
	}

	res := make([]map[string]any, 0, len(cases))
	for _, c := range cases {
		p := &model.Post{Type: c.postType}
		model.ParseMessageAttachment(p, c.attachments)

		propsBlob, _ := json.Marshal(p.GetProps())
		res = append(res, map[string]any{
			"name":      c.name,
			"in_type":   c.postType,
			"out_type":  p.Type,
			"out_props": json.RawMessage(propsBlob),
		})
	}
	return res
}

// maEqualsFromJSON is the comparison that actually happens on a server: both sides decoded from
// JSON, where encoding/json has collapsed every number to float64. That erases the Go-type
// distinction `maEqualsAll` probes, so `1` and `1.0` compare EQUAL here while int64(1) and
// int(1) do not there. serde_json keeps integers and floats apart, so this is the corpus the
// Rust port has to match.
func maEqualsFromJSON() []map[string]any {
	pairs := [][2]string{
		{`{"ts":1}`, `{"ts":1}`},
		{`{"ts":1}`, `{"ts":1.0}`},
		{`{"ts":1}`, `{"ts":2}`},
		{`{"ts":1}`, `{"ts":"1"}`},
		{`{"ts":null}`, `{"ts":null}`},
		{`{"ts":1}`, `{"ts":null}`},
		{`{"ts":1e2}`, `{"ts":100}`},
		{`{"fields":[{"value":1}]}`, `{"fields":[{"value":1.0}]}`},
		{`{"fields":[{"value":1}]}`, `{"fields":[{"value":2}]}`},
		{`{"fields":[{"value":"a"}]}`, `{"fields":[{"value":"a"}]}`},
		{`{"fields":[{"value":{"a":1}}]}`, `{"fields":[{"value":{"a":1.0}}]}`},
		{`{"fields":[{"value":[1,2]}]}`, `{"fields":[{"value":[1.0,2.0]}]}`},
		{`{"fields":[{"short":true}]}`, `{"fields":[{"short":"true"}]}`},
	}

	res := make([]map[string]any, 0, len(pairs))
	for _, p := range pairs {
		var a, b model.MessageAttachment
		if err := json.Unmarshal([]byte(p[0]), &a); err != nil {
			panic(err)
		}
		if err := json.Unmarshal([]byte(p[1]), &b); err != nil {
			panic(err)
		}
		row := map[string]any{"a": p[0], "b": p[1]}
		func() {
			defer func() {
				if r := recover(); r != nil {
					row["panicked"] = true
				}
			}()
			row["equals"] = a.Equals(&b)
			row["panicked"] = false
		}()
		res = append(res, row)
	}
	return res
}

// goFormatVFloats pins fmt.Sprintf("%v") for float64, which is %g — Go's shortest round-tripping
// representation, switching to exponent form outside a specific range. Rust's f64 Display never
// uses exponent form at all, so this cannot be reproduced by formatting alone.
//
// Every value here is one encoding/json could actually produce, since a decoded JSON number is
// always a float64.
func goFormatVFloats() []map[string]any {
	values := []float64{
		0, 1, -1, 1.5, -2.5, 0.5, 100, 1e6, 1e20, 1e21, 1e22, -1e21,
		0.0001, 0.00001, 0.000001, 1e-7, 1e-20,
		3.14159265358979, 2.718281828459045,
		123456789, 1234567890123456789, 12345678901234567890,
		0.1, 0.2, 0.3, 1.0 / 3.0,
		9007199254740993, 1e100, 1e-100,
		1.7976931348623157e308, 5e-324,
		-0.0, 1e15, 1e16, 1e17, 999999999999999999999,
	}
	res := make([]map[string]any, 0, len(values))
	for _, v := range values {
		blob, _ := json.Marshal(v)
		res = append(res, map[string]any{
			"json":      string(blob),
			"formatted": fmt.Sprintf("%v", v),
		})
	}
	return res
}

// sprintfVFromJSON is the %v corpus that matters: values decoded from JSON, which is how every
// real field value arrives. It covers the container cases the Go-native corpus does not — a
// nil inside a slice or map prints as `<nil>`, and strings are printed bare, so a value
// containing a space is indistinguishable from two elements.
func sprintfVFromJSON() []map[string]any {
	blobs := []string{
		`"x"`, `""`, `"a b"`, `"map[a:1]"`,
		`0`, `1`, `-1`, `1.5`, `1e21`, `1e-7`, `123456789`,
		`true`, `false`,
		`[]`, `[1,2]`, `["a","b"]`, `[null]`, `[null,1]`, `[[1,2],[3]]`, `["a b","c"]`,
		`{}`, `{"a":1}`, `{"z":1,"a":2}`, `{"a":null}`, `{"a":{"b":[1,null]}}`,
		`{"a b":"c d"}`,
	}

	res := make([]map[string]any, 0, len(blobs))
	for _, blob := range blobs {
		var v any
		if err := json.Unmarshal([]byte(blob), &v); err != nil {
			panic(err)
		}
		row := map[string]any{"json": blob}
		if v == nil {
			row["formatted"] = nil
		} else {
			row["formatted"] = fmt.Sprintf("%v", v)
		}
		res = append(res, row)
	}
	return res
}
