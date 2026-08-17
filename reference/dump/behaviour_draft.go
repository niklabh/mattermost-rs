package main

// Behavioural oracle for model/draft.go, written to fixtures/behaviour_draft.json.
//
// `Draft` reads like a trimmed-down `Post` and the temptation is to port it as one. Five things
// differ, and each is measured here rather than assumed:
//
//  1. **The message-length check runs FIRST**, before `create_at`. `Post.IsValid` checks the id
//     and the timestamps before it looks at the message; `Draft.IsValid` checks the message and
//     only then calls `BaseIsValid`. So a wholly zero draft with an over-long message reports
//     `message_length`, not `create_at`, and the error-id a client sees for the same broken
//     object differs between the two types.
//
//  2. **`Where` is `Drafts.IsValid` — plural** — for every branch, including the ones in
//     `BaseIsValid`. Recorded per case so the plural cannot be tidied away.
//
//  3. **Every detail is `channelid=…`, except the four that are empty.** `user_id`, `channel_id`
//     and `root_id` pass `""`; the rest pass the channel id. `Post` uses `id=` throughout.
//
//  4. **`Props` has no `omitempty` but `FileIds`, `Metadata` and `Priority` do.** So a nil props
//     is `"props":null` on the wire while a nil *or empty* file-id list vanishes entirely. Both
//     halves are driven, because a `Vec` with the wrong skip predicate passes a round-trip test.
//
//  5. **`Priority` is a bare `StringInterface` here**, not the typed `*PostPriority` that
//     `PostMetadata` carries. It is measured by the props cap and never validated, so a draft can
//     hold a priority no post could.
//
// `BaseIsValid` is recorded separately from `IsValid` for every case, because it is exported and
// the store calls it directly — porting it as a private helper of `is_valid` would lose a public
// entry point whose answers differ (it skips the message check entirely).

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeDraftBehaviourFixture(outDir string) error {
	out := map[string]any{
		"wire":            draftWireAll(),
		"is_valid":        draftIsValidAll(),
		"pre_save":        draftPreSaveAll(),
		"pre_commit":      draftPreCommitAll(),
		"props_accessors": draftPropsAccessorsAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_draft.json"), append(blob, '\n'), 0o644)
}

// --- wire ----------------------------------------------------------------------------------

// draftWireAll decodes a document and re-marshals it, recording the nil-ness of the four
// reference fields alongside. The JSON alone cannot distinguish a nil `FileIds` from an empty
// one — omitempty drops both — and `PreCommit` branches on exactly that distinction.
func draftWireAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"empty_object", `{}`},
		{"all_fields", `{"create_at":1700000000000,"update_at":1700000001000,"delete_at":1700000002000,` +
			`"user_id":"` + idA + `","channel_id":"` + idB + `","root_id":"` + idC + `",` +
			`"message":"hello","type":"custom_draft","props":{"a":"b"},"file_ids":["f1","f2"],` +
			`"metadata":{"emojis":[{"name":"smile"}]},"priority":{"priority":"urgent"}}`},

		// props has NO omitempty: nil is `null`, empty is `{}`, and the key is never absent.
		{"props_absent", `{"message":"m"}`},
		{"props_null", `{"props":null}`},
		{"props_empty", `{"props":{}}`},
		{"props_nested", `{"props":{"z":1,"a":{"b":[true,null,2.5]}}}`},

		// file_ids HAS omitempty, so nil and empty are indistinguishable on the way out.
		{"file_ids_absent", `{"message":"m"}`},
		{"file_ids_null", `{"file_ids":null}`},
		{"file_ids_empty", `{"file_ids":[]}`},
		{"file_ids_dupes", `{"file_ids":["b","a","b"]}`},

		// priority is a bare map with omitempty — same shape as file_ids, unlike Post's typed one.
		{"priority_null", `{"priority":null}`},
		{"priority_empty", `{"priority":{}}`},
		{"priority_arbitrary", `{"priority":{"priority":"urgent","requested_ack":true,"nonsense":[1]}}`},

		{"metadata_null", `{"metadata":null}`},
		{"metadata_empty", `{"metadata":{}}`},

		{"type_empty", `{"type":""}`},
		{"escapes", `{"message":"a<b>c&d","props":{"k":"<script>"}}`},
		{"unknown_key", `{"nope":1,"message":"m"}`},
		// Go leaves the destination untouched on null; serde does not ([D-057]).
		{"null_scalars", `{"message":null,"create_at":null,"user_id":null}`},
	}

	var res []map[string]any
	for _, c := range docs {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			var d model.Draft
			if err := json.Unmarshal([]byte(c.doc), &d); err != nil {
				row["err"] = err.Error()
				return
			}
			row["err"] = nil
			row["out"] = mustMarshal(&d)
			row["props_nil"] = d.Props == nil
			row["file_ids_nil"] = d.FileIds == nil
			row["priority_nil"] = d.Priority == nil
			row["metadata_nil"] = d.Metadata == nil
		})
		res = append(res, row)
	}

	res = append(res, map[string]any{
		"name":         "zero_value",
		"in":           "",
		"err":          nil,
		"panicked":     false,
		"out":          mustMarshal(&model.Draft{}),
		"props_nil":    true,
		"file_ids_nil": true,
		"priority_nil": true,
		"metadata_nil": true,
	})
	return res
}

// --- IsValid / BaseIsValid -----------------------------------------------------------------

// draftPad describes a value too large to commit. `PostPropsMaxRunes` is 800,000, so the five
// cap-crossing cases would add ~4 MB to the fixture if their strings were embedded — and the
// string is pure padding, carrying no information the three numbers below do not. The marshalled
// draft holds `""` at `<field>.<key>`; the Rust side substitutes `Prefix + Fill*Count` before
// decoding. See [D-032]'s sibling concern: a fixture nobody can open is a fixture nobody checks.
type draftPad struct {
	Field  string `json:"field"` // "props" or "priority"
	Key    string `json:"key"`
	Prefix string `json:"prefix"`
	Fill   string `json:"fill"`
	Count  int    `json:"count"`
}

type draftValidCase struct {
	Name         string          `json:"name"`
	Draft        json.RawMessage `json:"draft"`
	Pad          *draftPad       `json:"pad,omitempty"`
	MaxDraftSize int             `json:"max_draft_size"`
	// IsValid(maxDraftSize).
	Where    string `json:"where"`
	ErrorID  string `json:"error_id"`
	Detailed string `json:"detailed"`
	Status   int    `json:"status"`
	// BaseIsValid(), which skips the message check. Exported, so it is its own entry point.
	BaseWhere    string `json:"base_where"`
	BaseErrorID  string `json:"base_error_id"`
	BaseDetailed string `json:"base_detailed"`
}

func draftIsValidAll() []draftValidCase {
	type mut struct {
		name string
		max  int
		fn   func(d *model.Draft)
	}
	const dflt = 4000

	// {"a":"<pad>"} is 8 runes of framing plus the value; the cap is measured over Go's JSON,
	// so an escaped character costs six runes, not one.
	const propsFraming = 8

	muts := []mut{
		{"valid", dflt, func(d *model.Draft) {}},

		// The message check runs before every other one — this is the whole point of the file.
		{"message_at_limit", 10, func(d *model.Draft) { d.Message = repeat("a", 10) }},
		{"message_over_limit", 10, func(d *model.Draft) { d.Message = repeat("a", 11) }},
		{"message_runes_not_bytes", 10, func(d *model.Draft) { d.Message = repeat("é", 10) }},
		{"message_runes_over", 10, func(d *model.Draft) { d.Message = repeat("é", 11) }},
		{"message_max_zero", 0, func(d *model.Draft) { d.Message = "a" }},
		{"message_max_zero_empty", 0, func(d *model.Draft) { d.Message = "" }},
		{"message_max_negative", -1, func(d *model.Draft) { d.Message = "" }},
		// Over-long message AND a zero create_at: which id wins tells us the check order.
		{"message_over_and_create_at_zero", 10, func(d *model.Draft) {
			d.Message = repeat("a", 11)
			d.CreateAt = 0
		}},
		{"message_over_and_bad_user_id", 10, func(d *model.Draft) {
			d.Message = repeat("a", 11)
			d.UserId = "nope"
		}},

		{"create_at_zero", dflt, func(d *model.Draft) { d.CreateAt = 0 }},
		{"create_at_negative", dflt, func(d *model.Draft) { d.CreateAt = -1 }},
		{"update_at_zero", dflt, func(d *model.Draft) { d.UpdateAt = 0 }},
		{"update_at_negative", dflt, func(d *model.Draft) { d.UpdateAt = -1 }},
		// delete_at is deprecated and never checked, at any value.
		{"delete_at_zero", dflt, func(d *model.Draft) { d.DeleteAt = 0 }},
		{"delete_at_negative", dflt, func(d *model.Draft) { d.DeleteAt = -1 }},

		{"user_id_empty", dflt, func(d *model.Draft) { d.UserId = "" }},
		{"user_id_nonsense", dflt, func(d *model.Draft) { d.UserId = "nope" }},
		{"user_id_25", dflt, func(d *model.Draft) { d.UserId = repeat("a", 25) }},
		{"user_id_27", dflt, func(d *model.Draft) { d.UserId = repeat("a", 27) }},
		// IsValidId is 26 *bytes* of letters/digits, so 13 two-byte letters pass.
		{"user_id_13_two_byte", dflt, func(d *model.Draft) { d.UserId = repeat("é", 13) }},

		{"channel_id_empty", dflt, func(d *model.Draft) { d.ChannelId = "" }},
		{"channel_id_nonsense", dflt, func(d *model.Draft) { d.ChannelId = "nope" }},

		// RootId is optional but must be a real id when set.
		{"root_id_empty", dflt, func(d *model.Draft) { d.RootId = "" }},
		{"root_id_nonsense", dflt, func(d *model.Draft) { d.RootId = "nope" }},

		// Type is NEVER validated — no accepted set, no prefix rule, unlike Post.
		{"type_empty", dflt, func(d *model.Draft) { d.Type = "" }},
		{"type_nonsense", dflt, func(d *model.Draft) { d.Type = "system_nope" }},
		{"type_long", dflt, func(d *model.Draft) { d.Type = repeat("x", 1000) }},

		// FileIds is measured through ArrayToJSON. Nil marshals as `null` — four runes, not two.
		{"file_ids_nil", dflt, func(d *model.Draft) { d.FileIds = nil }},
		{"file_ids_empty", dflt, func(d *model.Draft) { d.FileIds = model.StringArray{} }},
		{"file_ids_at_limit", dflt, func(d *model.Draft) {
			d.FileIds = model.StringArray{repeat("a", model.PostFileidsMaxRunes-4)}
		}},
		{"file_ids_over_limit", dflt, func(d *model.Draft) {
			d.FileIds = model.StringArray{repeat("a", model.PostFileidsMaxRunes-3)}
		}},
		// The contents are never validated as ids.
		{"file_ids_nonsense", dflt, func(d *model.Draft) { d.FileIds = model.StringArray{"nope"} }},

		{"props_nil", dflt, func(d *model.Draft) { d.Props = nil }},
		{"props_empty", dflt, func(d *model.Draft) { d.Props = model.StringInterface{} }},
		{"props_at_limit", dflt, func(d *model.Draft) {
			d.Props = model.StringInterface{"a": repeat("x", model.PostPropsMaxRunes-propsFraming)}
		}},
		{"props_over_limit", dflt, func(d *model.Draft) {
			d.Props = model.StringInterface{"a": repeat("x", model.PostPropsMaxRunes-propsFraming+1)}
		}},
		// One `<` costs six runes, so the same string length crosses the cap when escaped.
		{"props_escaped_over_limit", dflt, func(d *model.Draft) {
			d.Props = model.StringInterface{"a": "<" + repeat("x", model.PostPropsMaxRunes-propsFraming-1)}
		}},

		// Priority shares the props cap — the same constant, checked a second time.
		{"priority_nil", dflt, func(d *model.Draft) { d.Priority = nil }},
		{"priority_empty", dflt, func(d *model.Draft) { d.Priority = model.StringInterface{} }},
		{"priority_at_limit", dflt, func(d *model.Draft) {
			d.Priority = model.StringInterface{"a": repeat("x", model.PostPropsMaxRunes-propsFraming)}
		}},
		{"priority_over_limit", dflt, func(d *model.Draft) {
			d.Priority = model.StringInterface{"a": repeat("x", model.PostPropsMaxRunes-propsFraming+1)}
		}},
		// Never validated as a priority: any shape passes.
		{"priority_nonsense", dflt, func(d *model.Draft) {
			d.Priority = model.StringInterface{"priority": 17, "requested_ack": "yes"}
		}},

		// Metadata is not validated at all, and is not measured by any cap.
		{"metadata_nil", dflt, func(d *model.Draft) { d.Metadata = nil }},
		{"metadata_empty", dflt, func(d *model.Draft) { d.Metadata = &model.PostMetadata{} }},
	}

	// The cases whose value is pure padding, described rather than embedded. Keyed by case name
	// so the corpus above stays a flat list of three-field literals.
	pads := map[string]*draftPad{
		"props_at_limit":           {"props", "a", "", "x", model.PostPropsMaxRunes - propsFraming},
		"props_over_limit":         {"props", "a", "", "x", model.PostPropsMaxRunes - propsFraming + 1},
		"props_escaped_over_limit": {"props", "a", "<", "x", model.PostPropsMaxRunes - propsFraming - 1},
		"priority_at_limit":        {"priority", "a", "", "x", model.PostPropsMaxRunes - propsFraming},
		"priority_over_limit":      {"priority", "a", "", "x", model.PostPropsMaxRunes - propsFraming + 1},
	}

	res := make([]draftValidCase, 0, len(muts))
	for _, m := range muts {
		d := newValidDraft()
		m.fn(d)

		// Validate the real draft, then blank the padding before marshalling it.
		validErr := d.IsValid(m.max)
		baseErr := d.BaseIsValid()

		pad := pads[m.name]
		if pad != nil {
			switch pad.Field {
			case "props":
				d.Props[pad.Key] = ""
			case "priority":
				d.Priority[pad.Key] = ""
			default:
				panic("unknown pad field " + pad.Field)
			}
		}

		blob, err := json.Marshal(d)
		if err != nil {
			panic(err)
		}
		c := draftValidCase{Name: m.name, Draft: blob, Pad: pad, MaxDraftSize: m.max}
		if validErr != nil {
			c.Where = validErr.Where
			c.ErrorID = validErr.Id
			c.Detailed = validErr.DetailedError
			c.Status = validErr.StatusCode
		}
		if baseErr != nil {
			c.BaseWhere = baseErr.Where
			c.BaseErrorID = baseErr.Id
			c.BaseDetailed = baseErr.DetailedError
		}
		res = append(res, c)
	}
	return res
}

// newValidDraft returns a draft that passes IsValid at any sane maxDraftSize. Built fresh per
// case rather than copied: Draft embeds a sync.RWMutex, so `d := *base` copies a lock.
func newValidDraft() *model.Draft {
	return &model.Draft{
		CreateAt:  1700000000000,
		UpdateAt:  1700000001000,
		DeleteAt:  1700000002000,
		UserId:    idA,
		ChannelId: idB,
		RootId:    idC,
		Message:   "hello",
		Type:      "",
		Props:     model.StringInterface{"a": "b"},
		FileIds:   model.StringArray{idA},
		Priority:  model.StringInterface{"priority": "urgent"},
	}
}

// --- PreSave / PreCommit ---------------------------------------------------------------------

// PreSave calls GetMillis, which may not be recorded (see D-032). What is recorded are the
// invariants: whether create_at was taken from the clock, whether update_at tracks it, and what
// PreCommit did to the collections.
type draftHookCase struct {
	Name string          `json:"name"`
	In   json.RawMessage `json:"in"`

	CreateAtWasKept  bool  `json:"create_at_was_kept"`
	CreateAtValue    int64 `json:"create_at_value"` // 0 unless it was kept
	UpdateAtEqualsCr bool  `json:"update_at_equals_create_at"`
	UpdateAtMoved    bool  `json:"update_at_moved"`
	DeleteAtOut      int64 `json:"delete_at_out"`

	PropsNilOut    bool            `json:"props_nil_out"`
	PropsOut       json.RawMessage `json:"props_out"`
	FileIdsNilOut  bool            `json:"file_ids_nil_out"`
	FileIdsOut     []string        `json:"file_ids_out"`
	PriorityNilOut bool            `json:"priority_nil_out"`
}

func draftPreSaveAll() []draftHookCase {
	return draftHooksOver(func(d *model.Draft) { d.PreSave() }, true)
}

func draftPreCommitAll() []draftHookCase {
	return draftHooksOver(func(d *model.Draft) { d.PreCommit() }, false)
}

func draftHooksOver(run func(*model.Draft), touchesTime bool) []draftHookCase {
	docs := []struct{ name, doc string }{
		{"all_zero", `{}`},
		{"create_at_set", `{"create_at":1700000000000,"update_at":1700000000000}`},
		{"create_at_set_delete_at_set", `{"create_at":1700000000000,"delete_at":1700000005000}`},
		{"create_at_negative", `{"create_at":-1}`},
		{"delete_at_only", `{"delete_at":1700000005000}`},

		{"props_null", `{"create_at":1,"props":null}`},
		{"props_empty", `{"create_at":1,"props":{}}`},
		{"props_set", `{"create_at":1,"props":{"b":2,"a":1}}`},

		{"file_ids_null", `{"create_at":1,"file_ids":null}`},
		{"file_ids_empty", `{"create_at":1,"file_ids":[]}`},
		// RemoveDuplicateStrings SORTS as well as de-duplicating.
		{"file_ids_unsorted", `{"create_at":1,"file_ids":["c","a","b"]}`},
		{"file_ids_dupes", `{"create_at":1,"file_ids":["b","a","b","a","b"]}`},
		{"file_ids_all_same", `{"create_at":1,"file_ids":["a","a","a"]}`},
		{"file_ids_single", `{"create_at":1,"file_ids":["z"]}`},
		// Byte order, not collation: uppercase sorts before lowercase.
		{"file_ids_case", `{"create_at":1,"file_ids":["b","A","a","B"]}`},
		{"file_ids_empty_string", `{"create_at":1,"file_ids":["","a",""]}`},

		// PreCommit never touches Priority, so a nil one stays nil.
		{"priority_null", `{"create_at":1,"priority":null}`},
		{"priority_set", `{"create_at":1,"priority":{"priority":"urgent"}}`},
	}

	res := make([]draftHookCase, 0, len(docs))
	for _, c := range docs {
		var d model.Draft
		if err := json.Unmarshal([]byte(c.doc), &d); err != nil {
			panic(err)
		}
		inCreate, inUpdate := d.CreateAt, d.UpdateAt

		run(&d)

		out := draftHookCase{
			Name:           c.name,
			In:             json.RawMessage(c.doc),
			DeleteAtOut:    d.DeleteAt,
			PropsNilOut:    d.GetProps() == nil,
			PropsOut:       json.RawMessage(mustMarshal(d.GetProps())),
			FileIdsNilOut:  d.FileIds == nil,
			FileIdsOut:     []string(d.FileIds),
			PriorityNilOut: d.Priority == nil,
		}
		if touchesTime {
			out.CreateAtWasKept = d.CreateAt == inCreate && inCreate != 0
			if out.CreateAtWasKept {
				out.CreateAtValue = d.CreateAt
			}
			out.UpdateAtEqualsCr = d.UpdateAt == d.CreateAt
			out.UpdateAtMoved = d.UpdateAt != inUpdate
		} else {
			// PreCommit touches no timestamp, so every one of these must come back unchanged.
			out.CreateAtWasKept = d.CreateAt == inCreate
			out.CreateAtValue = d.CreateAt
			out.UpdateAtEqualsCr = d.UpdateAt == d.CreateAt
			out.UpdateAtMoved = d.UpdateAt != inUpdate
		}
		res = append(res, out)
	}
	return res
}

// --- GetProps / SetProps ---------------------------------------------------------------------

// The accessors exist only because Go needs a mutex around the map. They are recorded anyway to
// pin the one observable fact: SetProps stores the argument as-is, nil included, so it can put a
// draft back into the state PreCommit exists to leave.
func draftPropsAccessorsAll() []map[string]any {
	cases := []struct {
		name string
		set  model.StringInterface
	}{
		{"set_nil", nil},
		{"set_empty", model.StringInterface{}},
		{"set_value", model.StringInterface{"a": "b"}},
	}

	var res []map[string]any
	for _, c := range cases {
		d := &model.Draft{Props: model.StringInterface{"pre": "existing"}}
		d.SetProps(c.set)
		res = append(res, map[string]any{
			"name":     c.name,
			"nil_out":  d.GetProps() == nil,
			"json_out": mustMarshal(d.GetProps()),
			"draft":    mustMarshal(d),
		})
	}
	return res
}
