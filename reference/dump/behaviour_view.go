package main

// Behavioural oracle for model.View and the kanban props family, written to
// fixtures/behaviour_view.json.
//
// # The risky part is a validator that reports `encoding/json`'s error text
//
//	kanban, err := KanbanPropsFromProps(props)
//	if err != nil {
//		return NewAppError("View.IsValid", "…kanban_invalid.app_error", nil, err.Error(), …)
//
// `err.Error()` is Go's own unmarshal message — something like
// `json: cannot unmarshal string into Go struct field KanbanProps.group_by of type model.KanbanGroupBy`
// — and it lands in `DetailedError`, which is on the wire. Reproducing that text in Rust means
// reproducing `encoding/json`'s error formatting, including the **Go type names**. So the corpus
// records the exact string for every input that produces one, and the port decides from data
// whether that is reachable enough to be worth it.
//
// # `KanbanPropsFromProps` is a JSON round trip, so Go's decoder rules apply
//
// It marshals the `StringInterface` and unmarshals into a struct. Unknown keys are **ignored**,
// missing keys are left zero, and a type mismatch fails — but Go's decoder is not all-or-nothing,
// so what survives a partial failure is worth recording ([D-026] is the same shape of problem).
//
// # `IsValid` always requires kanban props
//
// The only accepted `Type` is `kanban`, and `validateViewProps` routes every kanban view to
// `validateKanbanProps`, which rejects a nil map. But `View.Props` carries `omitempty`, so a view
// with no props is a perfectly ordinary wire shape that can never validate.
//
// # The title check reads the untrimmed string for length and the trimmed one for emptiness
//
//	if strings.TrimSpace(o.Title) == "" || utf8.RuneCountInString(o.Title) > ViewTitleMaxRunes
//
// So the rune count includes leading and trailing whitespace. Driven at both boundaries, and the
// whitespace definition itself is swept — `strings.TrimSpace` uses `unicode.IsSpace`, which is the
// White_Space property and therefore includes NBSP and U+0085 but not U+200B.
//
// # Three branches carry i18n params
//
// The per-column errors pass `map[string]any{"Index": i}`. Every other branch passes nil. Recorded
// per branch, because `Params` is the only place in this file where two errors with the same id
// differ.
//
// Determinism: `PreSave` mints an id and reads the clock, so those are recorded as properties
// rather than values. No rand, no time elsewhere.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"unsafe"

	"github.com/mattermost/mattermost/server/public/model"
)

// vwUnexportedParams reads AppError's `params` field, which Go does not export.
//
// That is itself the finding: `validateKanbanProps` passes `map[string]any{"Index": i}` for its
// three per-column branches, and **no caller outside the model package can see it**. It feeds
// `Translate` alone, and with no i18n bundle registered `Translate` sets `Message = Id` and never
// reads it. So the value is unobservable in production and only a reflective read can pin it.
//
// Recorded rather than skipped because the port sets the field, and without this nothing would
// catch it setting the wrong index — the branches are otherwise identical apart from their id.
func vwUnexportedParams(err *model.AppError) map[string]any {
	if err == nil {
		return nil
	}
	field := reflect.ValueOf(err).Elem().FieldByName("params")
	if !field.IsValid() {
		return nil
	}
	readable := reflect.NewAt(field.Type(), unsafe.Pointer(field.UnsafeAddr())).Elem()
	params, _ := readable.Interface().(map[string]any)
	return params
}

func writeViewBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":          viewConstants(),
		"is_valid":           viewIsValidAll(),
		"kanban_props":       viewKanbanPropsAll(),
		"to_props":           viewToPropsAll(),
		"pre_save":           viewPreSaveAll(),
		"patch":              viewPatchAll(),
		"clone":              viewCloneProbe(),
		"auditable":          viewAuditableProbe(),
		"trim_space_charset": viewTrimSpaceCharset(),
		"wire":               viewWireProbes(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_view.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

func viewConstants() map[string]any {
	return map[string]any{
		"ViewTypeKanban":               string(model.ViewTypeKanban),
		"ViewTitleMaxRunes":            model.ViewTitleMaxRunes,
		"ViewDescriptionMaxRunes":      model.ViewDescriptionMaxRunes,
		"MaxViewsPerChannel":           model.MaxViewsPerChannel,
		"BoardsPropertyGroupName":      model.BoardsPropertyGroupName,
		"BoardsPropertyFieldNameBoard": model.BoardsPropertyFieldNameBoard,
		"BoardsPropertyFieldAssignee":  model.BoardsPropertyFieldAssignee,
		"BoardsPropertyFieldStatus":    model.BoardsPropertyFieldStatus,
		"BoardsStatusOptionTodo":       model.BoardsStatusOptionTodo,
		"BoardsStatusOptionInProgress": model.BoardsStatusOptionInProgress,
		"BoardsStatusOptionComplete":   model.BoardsStatusOptionComplete,
		"BoardsStatusColorTodo":        model.BoardsStatusColorTodo,
		"BoardsStatusColorInProgress":  model.BoardsStatusColorInProgress,
		"BoardsStatusColorComplete":    model.BoardsStatusColorComplete,
		"MaxKanbanColumns":             model.MaxKanbanColumns,
		"ViewQueryDefaultPerPage":      model.ViewQueryDefaultPerPage,
		"ViewQueryMaxPerPage":          model.ViewQueryMaxPerPage,
	}
}

const vwID = "abcdefghijklmnopqrstuvwxyz"
const vwChannelID = "zyxwvutsrqponmlkjihgfedcba"
const vwCreatorID = "0123456789abcdefghijklmnop"
const vwFieldID = "aaaaaaaaaaaaaaaaaaaaaaaaaa"
const vwColumnID = "bbbbbbbbbbbbbbbbbbbbbbbbbb"

// vwKanbanProps builds a valid props map the way the app layer would: through ToProps.
func vwKanbanProps() model.StringInterface {
	kp := &model.KanbanProps{
		GroupBy: model.KanbanGroupBy{
			FieldID: vwFieldID,
			Columns: []model.KanbanColumn{
				{ID: vwColumnID, Name: "Todo", OptionIDs: []string{"opt-1"}},
			},
		},
	}
	props, err := kp.ToProps()
	if err != nil {
		panic("valid kanban props must convert: " + err.Error())
	}
	return props
}

func vwValidView() model.View {
	return model.View{
		Id:          vwID,
		ChannelId:   vwChannelID,
		Type:        model.ViewTypeKanban,
		CreatorId:   vwCreatorID,
		Title:       "Sprint board",
		Description: "The team's kanban",
		SortOrder:   3,
		Props:       vwKanbanProps(),
		CreateAt:    1700000000000,
		UpdateAt:    1700000001000,
	}
}

// vwPropsFromJSON builds a StringInterface from a literal, so the corpus can express shapes that
// ToProps could never produce.
func vwPropsFromJSON(raw string) model.StringInterface {
	var m model.StringInterface
	if err := json.Unmarshal([]byte(raw), &m); err != nil {
		panic("corpus literal must parse: " + raw + ": " + err.Error())
	}
	return m
}

func vwErrEntry(name string, err *model.AppError) map[string]any {
	entry := map[string]any{"name": name, "ok": err == nil}
	if err != nil {
		entry["id"] = err.Id
		entry["where"] = err.Where
		entry["status"] = err.StatusCode
		entry["detailed_error"] = err.DetailedError
		params := vwUnexportedParams(err)
		entry["params"] = params
		entry["has_params"] = params != nil
	}
	return entry
}

func viewIsValidAll() []map[string]any {
	manyColumns := func(n int) model.StringInterface {
		cols := make([]model.KanbanColumn, 0, n)
		for i := 0; i < n; i++ {
			cols = append(cols, model.KanbanColumn{
				ID:        vwColumnID,
				Name:      fmt.Sprintf("Col %d", i),
				OptionIDs: []string{"opt"},
			})
		}
		kp := &model.KanbanProps{GroupBy: model.KanbanGroupBy{FieldID: vwFieldID, Columns: cols}}
		props, err := kp.ToProps()
		if err != nil {
			panic(err)
		}
		return props
	}

	corpus := []struct {
		name string
		mut  func(*model.View)
	}{
		{"valid", func(*model.View) {}},
		{"bad_id", func(v *model.View) { v.Id = "nope" }},
		{"empty_id", func(v *model.View) { v.Id = "" }},
		{"bad_channel_id", func(v *model.View) { v.ChannelId = "nope" }},
		{"bad_creator_id", func(v *model.View) { v.CreatorId = "nope" }},
		{"empty_type", func(v *model.View) { v.Type = "" }},
		{"unknown_type", func(v *model.View) { v.Type = "list" }},
		{"uppercase_type", func(v *model.View) { v.Type = "Kanban" }},
		{"empty_title", func(v *model.View) { v.Title = "" }},
		{"whitespace_title", func(v *model.View) { v.Title = "   \t\n  " }},
		{"nbsp_only_title", func(v *model.View) { v.Title = "\u00a0\u00a0" }},
		{"zwsp_only_title", func(v *model.View) { v.Title = "\u200b" }},
		{"title_at_cap", func(v *model.View) { v.Title = strings.Repeat("t", model.ViewTitleMaxRunes) }},
		{"title_over_cap", func(v *model.View) { v.Title = strings.Repeat("t", model.ViewTitleMaxRunes+1) }},
		// The length counts RUNES of the untrimmed string, so padding pushes a short title over.
		{"title_padded_over_cap", func(v *model.View) {
			v.Title = strings.Repeat(" ", model.ViewTitleMaxRunes) + "abc"
		}},
		{"title_multibyte_at_cap", func(v *model.View) {
			v.Title = strings.Repeat("\u00e9", model.ViewTitleMaxRunes)
		}},
		{"title_multibyte_over_cap", func(v *model.View) {
			v.Title = strings.Repeat("\u00e9", model.ViewTitleMaxRunes+1)
		}},
		{"description_at_cap", func(v *model.View) {
			v.Description = strings.Repeat("d", model.ViewDescriptionMaxRunes)
		}},
		{"description_over_cap", func(v *model.View) {
			v.Description = strings.Repeat("d", model.ViewDescriptionMaxRunes+1)
		}},
		{"empty_description_is_fine", func(v *model.View) { v.Description = "" }},
		{"zero_create_at", func(v *model.View) { v.CreateAt = 0 }},
		{"zero_update_at", func(v *model.View) { v.UpdateAt = 0 }},
		{"negative_create_at", func(v *model.View) { v.CreateAt = -1 }},
		{"negative_sort_order", func(v *model.View) { v.SortOrder = -5 }},

		// --- props ------------------------------------------------------------------------------
		{"nil_props", func(v *model.View) { v.Props = nil }},
		{"empty_props", func(v *model.View) { v.Props = model.StringInterface{} }},
		{"props_missing_group_by", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"something":"else"}`)
		}},
		{"props_group_by_is_a_string", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":"nope"}`)
		}},
		{"props_group_by_is_a_number", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":42}`)
		}},
		{"props_columns_is_a_string", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":"` + vwFieldID + `","columns":"nope"}}`)
		}},
		{"props_field_id_is_a_number", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":42,"columns":[]}}`)
		}},
		{"props_bad_field_id", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":"nope","columns":[]}}`)
		}},
		{"props_empty_columns", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":"` + vwFieldID + `","columns":[]}}`)
		}},
		{"props_null_columns", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":"` + vwFieldID + `","columns":null}}`)
		}},
		{"props_columns_at_max", func(v *model.View) { v.Props = manyColumns(model.MaxKanbanColumns) }},
		{"props_columns_over_max", func(v *model.View) { v.Props = manyColumns(model.MaxKanbanColumns + 1) }},
		{"props_bad_column_id", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":"` + vwFieldID +
				`","columns":[{"id":"nope","name":"Todo","option_ids":["o"]}]}}`)
		}},
		{"props_empty_column_name", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":"` + vwFieldID +
				`","columns":[{"id":"` + vwColumnID + `","name":"  ","option_ids":["o"]}]}}`)
		}},
		{"props_empty_column_options", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":"` + vwFieldID +
				`","columns":[{"id":"` + vwColumnID + `","name":"Todo","option_ids":[]}]}}`)
		}},
		// The Index param: the SECOND column is the broken one, so `Index` must be 1.
		{"props_second_column_bad", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":"` + vwFieldID +
				`","columns":[{"id":"` + vwColumnID + `","name":"Todo","option_ids":["o"]},` +
				`{"id":"nope","name":"Doing","option_ids":["o"]}]}}`)
		}},
		{"props_third_column_no_options", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":"` + vwFieldID +
				`","columns":[{"id":"` + vwColumnID + `","name":"A","option_ids":["o"]},` +
				`{"id":"` + vwColumnID + `","name":"B","option_ids":["o"]},` +
				`{"id":"` + vwColumnID + `","name":"C","option_ids":[]}]}}`)
		}},
		// Unknown keys inside the typed structs are ignored by encoding/json.
		{"props_extra_keys_ignored", func(v *model.View) {
			v.Props = vwPropsFromJSON(`{"group_by":{"field_id":"` + vwFieldID +
				`","columns":[{"id":"` + vwColumnID + `","name":"Todo","option_ids":["o"],"extra":1}],` +
				`"unknown":true},"top_level_extra":"x"}`)
		}},

		// --- ordering --------------------------------------------------------------------------
		{"bad_id_and_bad_props", func(v *model.View) {
			v.Id = "nope"
			v.Props = nil
		}},
		{"bad_type_and_bad_title", func(v *model.View) {
			v.Type = "list"
			v.Title = ""
		}},
		// A non-kanban type SKIPS props validation entirely — but the type check rejects it first,
		// so this is unreachable through IsValid. Recorded via validateViewProps' shape instead.
		{"zero_update_at_and_nil_props", func(v *model.View) {
			v.UpdateAt = 0
			v.Props = nil
		}},
	}

	out := make([]map[string]any, 0, len(corpus))
	for _, c := range corpus {
		v := vwValidView()
		c.mut(&v)
		out = append(out, vwErrEntry(c.name, v.IsValid()))
	}
	return out
}

// viewKanbanPropsAll records what KanbanPropsFromProps does with each shape, separately from
// IsValid — including the exact Go error text, which is what reaches DetailedError.
func viewKanbanPropsAll() []map[string]any {
	inputs := []struct {
		name string
		raw  string
	}{
		{"empty_object", `{}`},
		{"unknown_keys", `{"a":1,"b":"two"}`},
		{"group_by_string", `{"group_by":"nope"}`},
		{"group_by_number", `{"group_by":42}`},
		{"group_by_array", `{"group_by":[]}`},
		{"group_by_null", `{"group_by":null}`},
		{"group_by_empty", `{"group_by":{}}`},
		{"field_id_number", `{"group_by":{"field_id":42}}`},
		{"columns_string", `{"group_by":{"field_id":"x","columns":"nope"}}`},
		{"columns_null", `{"group_by":{"field_id":"x","columns":null}}`},
		{"columns_empty", `{"group_by":{"field_id":"x","columns":[]}}`},
		{"column_option_ids_string", `{"group_by":{"field_id":"x","columns":[{"option_ids":"nope"}]}}`},
		{"column_id_number", `{"group_by":{"field_id":"x","columns":[{"id":7}]}}`},
		{"group_by_bool", `{"group_by":true}`},
		{"field_id_bool", `{"group_by":{"field_id":false}}`},
		{"field_id_object", `{"group_by":{"field_id":{}}}`},
		{"field_id_null", `{"group_by":{"field_id":null,"columns":[]}}`},
		{"columns_object", `{"group_by":{"columns":{}}}`},
		{"columns_element_string", `{"group_by":{"columns":["nope"]}}`},
		{"columns_element_null", `{"group_by":{"columns":[null]}}`},
		{"column_name_number", `{"group_by":{"columns":[{"name":1}]}}`},
		{"option_ids_element_number", `{"group_by":{"columns":[{"option_ids":[1]}]}}`},
		{"option_ids_element_null", `{"group_by":{"columns":[{"option_ids":[null]}]}}`},
		{"option_ids_null", `{"group_by":{"columns":[{"option_ids":null}]}}`},
		// Two type errors at once: encoding/json keeps only the FIRST, in document order — and
		// the document is the marshalled StringInterface, whose keys Go sorts.
		{"two_errors_columns_and_field_id", `{"group_by":{"field_id":1,"columns":"nope"}}`},
		{"two_errors_second_column", `{"group_by":{"columns":[{"id":"a"},{"id":2},{"name":3}]}}`},
		{"well_formed", `{"group_by":{"field_id":"x","columns":[{"id":"a","name":"n","option_ids":["o"]}]}}`},
	}

	var out []map[string]any
	for _, in := range inputs {
		props := vwPropsFromJSON(in.raw)
		kp, err := model.KanbanPropsFromProps(props)

		entry := map[string]any{
			"name":  in.name,
			"input": in.raw,
			"ok":    err == nil,
		}
		if err != nil {
			// The text that lands in DetailedError. It names GO types.
			entry["error"] = err.Error()
		} else {
			entry["field_id"] = kp.GroupBy.FieldID
			entry["column_count"] = len(kp.GroupBy.Columns)
			entry["columns_is_nil"] = kp.GroupBy.Columns == nil
			cols, _ := json.Marshal(kp.GroupBy.Columns)
			entry["columns_json"] = string(cols)
		}
		out = append(out, entry)
	}
	return out
}

func viewToPropsAll() []map[string]any {
	inputs := []struct {
		name string
		kp   model.KanbanProps
	}{
		{"zero", model.KanbanProps{}},
		{"field_only", model.KanbanProps{GroupBy: model.KanbanGroupBy{FieldID: vwFieldID}}},
		{"one_column", model.KanbanProps{GroupBy: model.KanbanGroupBy{
			FieldID: vwFieldID,
			Columns: []model.KanbanColumn{{ID: vwColumnID, Name: "Todo", OptionIDs: []string{"a", "b"}}},
		}}},
		{"empty_columns_slice", model.KanbanProps{GroupBy: model.KanbanGroupBy{
			FieldID: vwFieldID,
			Columns: []model.KanbanColumn{},
		}}},
		{"column_with_nil_options", model.KanbanProps{GroupBy: model.KanbanGroupBy{
			FieldID: vwFieldID,
			Columns: []model.KanbanColumn{{ID: vwColumnID, Name: "Todo"}},
		}}},
	}

	var out []map[string]any
	for _, in := range inputs {
		kp := in.kp
		props, err := kp.ToProps()
		entry := map[string]any{"name": in.name, "ok": err == nil}
		if err == nil {
			// Marshalled so the exact shape — including nulls — is assertable.
			blob, _ := json.Marshal(props)
			entry["props_json"] = string(blob)
		} else {
			entry["error"] = err.Error()
		}
		out = append(out, entry)
	}
	return out
}

func viewPreSaveAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.View
	}{
		{"all_zero", model.View{}},
		{"id_set", model.View{Id: vwID}},
		{"create_at_set", model.View{CreateAt: 1700000000000}},
		{"everything_set", model.View{Id: vwID, CreateAt: 1700000000000, UpdateAt: 5, DeleteAt: 9}},
		// DeleteAt is cleared unconditionally — PreSave un-deletes.
		{"deleted", model.View{Id: vwID, CreateAt: 1, DeleteAt: 1700000000000}},
	}

	var out []map[string]any
	for _, c := range corpus {
		v := c.in
		v.PreSave()
		entry := map[string]any{
			"name":               c.name,
			"in_id_empty":        c.in.Id == "",
			"in_create_at":       c.in.CreateAt,
			"id_is_generated":    c.in.Id == "",
			"create_at_uses_now": c.in.CreateAt == 0,
			"out_delete_at":      v.DeleteAt,
			// UpdateAt is set to CreateAt, whatever it was before.
			"update_at_equals_create_at": v.UpdateAt == v.CreateAt,
		}
		if c.in.Id != "" {
			entry["out_id"] = v.Id
		}
		if c.in.CreateAt != 0 {
			entry["out_create_at"] = v.CreateAt
			entry["out_update_at"] = v.UpdateAt
		}
		out = append(out, entry)
	}
	return out
}

func viewPatchAll() []map[string]any {
	str := func(s string) *string { return &s }
	num := func(n int) *int { return &n }
	props := func(p model.StringInterface) *model.StringInterface { return &p }

	corpus := []struct {
		name  string
		patch *model.ViewPatch
	}{
		{"nil_patch", nil},
		{"empty_patch", &model.ViewPatch{}},
		{"title", &model.ViewPatch{Title: str("New title")}},
		{"empty_title", &model.ViewPatch{Title: str("")}},
		{"description", &model.ViewPatch{Description: str("New description")}},
		{"empty_description", &model.ViewPatch{Description: str("")}},
		{"sort_order", &model.ViewPatch{SortOrder: num(42)}},
		{"zero_sort_order", &model.ViewPatch{SortOrder: num(0)}},
		{"props", &model.ViewPatch{Props: props(vwPropsFromJSON(`{"a":1}`))}},
		{"empty_props", &model.ViewPatch{Props: props(model.StringInterface{})}},
		{"nil_props_pointer_target", &model.ViewPatch{Props: props(nil)}},
		{"everything", &model.ViewPatch{
			Title:       str("T"),
			Description: str("D"),
			SortOrder:   num(7),
			Props:       props(vwPropsFromJSON(`{"z":true}`)),
		}},
	}

	var out []map[string]any
	for _, c := range corpus {
		v := vwValidView()
		v.Patch(c.patch)
		blob, _ := json.Marshal(v)
		out = append(out, map[string]any{
			"name":         c.name,
			"patch_is_nil": c.patch == nil,
			"out_json":     string(blob),
			"props_is_nil": v.Props == nil,
			"props_len":    len(v.Props),
		})
	}
	return out
}

// viewCloneProbe records that Clone copies the props MAP but shares its values.
//
// `maps.Copy` is shallow, so a nested map inside Props is aliased between the original and the
// clone. Same class as [D-015]; recorded so the Rust port's deep clone is a known divergence
// rather than an accident.
func viewCloneProbe() map[string]any {
	original := vwValidView()
	original.Props = vwPropsFromJSON(`{"nested":{"k":"v"},"flat":"x"}`)

	clone := original.Clone()
	clone.Title = "changed"
	clone.Props["flat"] = "y"

	nested, _ := original.Props["nested"].(map[string]any)
	if nested != nil {
		nested["k"] = "mutated-through-the-original"
	}
	cloneNested, _ := clone.Props["nested"].(map[string]any)

	var nilClone *model.View
	return map[string]any{
		"title_is_independent":         original.Title != clone.Title,
		"top_level_key_is_independent": original.Props["flat"] != clone.Props["flat"],
		// The shallow copy: mutating a nested map through the ORIGINAL is visible in the clone.
		"nested_map_is_shared":     cloneNested != nil && cloneNested["k"] == "mutated-through-the-original",
		"nil_receiver_returns_nil": nilClone.Clone() == nil,
	}
}

func viewAuditableProbe() map[string]any {
	v := vwValidView()
	a := v.Auditable()

	keys := make([]string, 0, len(a))
	for k := range a {
		keys = append(keys, k)
	}
	blob, _ := json.Marshal(a)

	return map[string]any{
		"key_count": len(keys),
		"json":      string(blob),
		// Title, Description, SortOrder and Props are NOT projected — the audit log records who
		// and when, not what the view says.
		"omits_title":       a["title"] == nil,
		"omits_description": a["description"] == nil,
		"omits_sort_order":  a["sort_order"] == nil,
		"omits_props":       a["props"] == nil,
		// `type` is a ViewType, a defined string type, so it marshals as its underlying string.
		"type_value": a["type"],
	}
}

// viewTrimSpaceCharset sweeps codepoints through the title's emptiness check.
//
// `strings.TrimSpace` uses `unicode.IsSpace` (the White_Space property). Rust's `str::trim` uses
// `char::is_whitespace`, which is the same property — but "the same property" is a claim, and this
// is the measurement. A title of exactly one probe character is empty-after-trim iff the character
// is whitespace.
func viewTrimSpaceCharset() []map[string]any {
	var points []rune
	for r := rune(0); r < 128; r++ {
		points = append(points, r)
	}
	points = append(points,
		'\u0085', '\u00a0', '\u1680', '\u2000', '\u2001', '\u2002',
		'\u2028', '\u2029', '\u3000', '\u200b', '\ufeff', '\u180e',
	)

	var out []map[string]any
	for _, r := range points {
		title := string(r)
		v := vwValidView()
		v.Title = title
		err := v.IsValid()

		out = append(out, map[string]any{
			"codepoint":      int(r),
			"trims_to_empty": strings.TrimSpace(title) == "",
			"title_rejected": err != nil && err.Id == "model.view.is_valid.title.app_error",
		})
	}
	return out
}

func viewWireProbes() []map[string]any {
	views := []struct {
		name string
		v    model.View
	}{
		{"zero", model.View{}},
		{"full", vwValidView()},
		{"no_description", func() model.View { v := vwValidView(); v.Description = ""; return v }()},
		{"nil_props", func() model.View { v := vwValidView(); v.Props = nil; return v }()},
		{"empty_props", func() model.View { v := vwValidView(); v.Props = model.StringInterface{}; return v }()},
		{"negative_sort_order", func() model.View { v := vwValidView(); v.SortOrder = -1; return v }()},
	}

	var out []map[string]any
	for _, c := range views {
		blob, _ := json.Marshal(c.v)
		out = append(out, map[string]any{
			"name": c.name,
			"json": string(blob),
			"keys": vwKeys(blob),
		})
	}

	// ViewsWithCount, whose Views field has no omitempty — so nil is `null`, not `[]`.
	for _, c := range []struct {
		name string
		vwc  model.ViewsWithCount
	}{
		{"views_with_count_nil", model.ViewsWithCount{TotalCount: 7}},
		{"views_with_count_empty", model.ViewsWithCount{Views: []*model.View{}, TotalCount: 0}},
		{"views_with_count_one", model.ViewsWithCount{
			Views: []*model.View{func() *model.View { v := vwValidView(); return &v }()}, TotalCount: 1,
		}},
		{"views_with_count_null_element", model.ViewsWithCount{Views: []*model.View{nil}, TotalCount: 1}},
	} {
		blob, _ := json.Marshal(c.vwc)
		out = append(out, map[string]any{"name": c.name, "json": string(blob), "keys": vwKeys(blob)})
	}

	// ViewPatch: every field is a pointer WITHOUT omitempty, so nil is `null` and all four keys
	// are always present.
	for _, c := range []struct {
		name string
		p    model.ViewPatch
	}{
		{"view_patch_zero", model.ViewPatch{}},
		{"view_patch_title_only", model.ViewPatch{Title: func() *string { s := "T"; return &s }()}},
	} {
		blob, _ := json.Marshal(c.p)
		out = append(out, map[string]any{"name": c.name, "json": string(blob), "keys": vwKeys(blob)})
	}

	// ViewQueryOpts has NO json tags at all — PascalCase keys, the wrangler.go shape.
	blob, _ := json.Marshal(model.ViewQueryOpts{Page: 2, PerPage: 50})
	out = append(out, map[string]any{
		"name": "view_query_opts", "json": string(blob), "keys": vwKeys(blob),
	})

	return out
}

func vwKeys(blob []byte) []string {
	var m map[string]json.RawMessage
	if err := json.Unmarshal(blob, &m); err != nil {
		return nil
	}
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	// Sorted, so the fixture is deterministic despite Go's map iteration ([D-032]).
	for i := 1; i < len(keys); i++ {
		for j := i; j > 0 && keys[j-1] > keys[j]; j-- {
			keys[j-1], keys[j] = keys[j], keys[j-1]
		}
	}
	return keys
}
