package main

// Behavioural oracle for model/post_info.go and model/post_attributes.go, written to
// fixtures/behaviour_post_info.json.
//
// Between them the two files are 27 lines: one wire struct with no methods, and two constants.
// There is no logic to measure, so this file exists for two narrower reasons.
//
//  1. **`PostInfo` has no `omitempty` on any of its eight fields**, which is unusual enough in
//     this package to be worth pinning rather than assuming. The zero value is eight keys, not
//     an empty object, and every one of them reaches the client.
//
//  2. **`ChannelType` is a defined string type, so `json.Unmarshal` accepts any string into it**
//     — including one no Go server would ever write. The Rust port models it as a `String` for
//     that reason ([`Channel`] made the same call), and the corpus drives a value outside the
//     accepted set so the decision is measured rather than argued.
//
// The two constants are recorded because a transcribed constant is exactly the kind of thing
// that drifts silently on an upstream bump — the same reasoning behaviour_version.go applies to
// the unexported `versions` table.

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writePostInfoBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants": map[string]any{
			"PostAttributesPropertyGroupName":          model.PostAttributesPropertyGroupName,
			"PostAttributesPropertyGroupSchemaVersion": model.PostAttributesPropertyGroupSchemaVersion,
		},
		"wire": postInfoWireAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_post_info.json"), append(blob, '\n'), 0o644)
}

func postInfoWireAll() []map[string]any {
	docs := []struct{ name, doc string }{
		{"empty_object", `{}`},
		{"all_fields", `{"channel_id":"c1","channel_type":"O","channel_display_name":"Town Square",` +
			`"has_joined_channel":true,"team_id":"t1","team_type":"O","team_display_name":"Core",` +
			`"has_joined_team":true}`},
		{"false_bools", `{"channel_id":"c1","has_joined_channel":false,"has_joined_team":false}`},
		// Every declared channel type, plus one that is not a channel type at all.
		{"type_private", `{"channel_type":"P"}`},
		{"type_direct", `{"channel_type":"D"}`},
		{"type_group", `{"channel_type":"G"}`},
		{"type_board", `{"channel_type":"BO"}`},
		{"type_unknown", `{"channel_type":"NOT_A_TYPE"}`},
		{"type_empty", `{"channel_type":""}`},
		// team_type is a plain string in Go, not a defined type — recorded side by side so the
		// asymmetry is visible.
		{"team_type_unknown", `{"team_type":"NOT_A_TYPE"}`},
		{"unknown_key", `{"nope":1,"channel_id":"c1"}`},
		// Go leaves the destination untouched on null, for every type; serde does not ([D-057]).
		{"null_scalars", `{"channel_id":null,"has_joined_team":null,"channel_type":null}`},
		{"escapes", `{"channel_display_name":"a<b>c&d","team_display_name":"\"quoted\""}`},
	}

	var res []map[string]any
	for _, c := range docs {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			var info model.PostInfo
			if err := json.Unmarshal([]byte(c.doc), &info); err != nil {
				row["err"] = err.Error()
				return
			}
			row["err"] = nil
			row["out"] = mustMarshal(&info)
			row["channel_type"] = string(info.ChannelType)
		})
		res = append(res, row)
	}

	// The zero value marshalled straight, which is what pins "no omitempty anywhere".
	res = append(res, map[string]any{
		"name":     "zero_value",
		"in":       "",
		"err":      nil,
		"panicked": false,
		"out":      mustMarshal(&model.PostInfo{}),
	})
	return res
}
