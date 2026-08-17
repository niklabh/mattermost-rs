package main

// Behavioural oracle for model/channel_view.go, written to fixtures/behaviour_channel_view.json.
//
// Fifteen lines, two structs, no methods, no constructor. The request and response of the
// mark-channel-read endpoint:
//
//	type ChannelView struct {
//	    ChannelId                 string `json:"channel_id"`
//	    PrevChannelId             string `json:"prev_channel_id"`
//	    CollapsedThreadsSupported bool   `json:"collapsed_threads_supported"`
//	}
//
//	type ChannelViewResponse struct {
//	    Status            string           `json:"status"`
//	    LastViewedAtTimes map[string]int64 `json:"last_viewed_at_times"`
//	}
//
// Nothing has `omitempty`, so every key is always present and the zero value of each struct is a
// full object rather than `{}`. That leaves one thing genuinely worth measuring and one worth
// checking:
//
//   - **`LastViewedAtTimes` is the first bare `map[string]int64` in the ported tree.** Nil and
//     empty are both reachable and differ on the wire (`null` versus `{}`), Go sorts the keys
//     when marshalling, and the *value* position has its own decode rules — which is where the
//     corpus spends most of its rows. `1.0` and `1e9` are rejected there exactly as they are in a
//     struct field, but `null` is **accepted**, and what it leaves behind is the question a port
//     has to answer: a zero entry, or no entry at all.
//
//   - **`CollapsedThreadsSupported` is the first bool the tree has decoded from client input in a
//     type with no validation.** Go rejects `"true"` and `1`; the corpus pins that rather than
//     assuming serde agrees.
//
// Determinism: fixed documents only. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"os"
	"path/filepath"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeChannelViewBehaviourFixture(outDir string) error {
	out := map[string]any{
		"view_wire":        channelViewWireAll(),
		"response_wire":    channelViewResponseWireAll(),
		"map_value_decode": channelViewMapValueAll(),
		"bool_decode":      channelViewBoolAll(),
		"map_marshal_wire": channelViewMapMarshalAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_channel_view.json"), append(blob, '\n'), 0o644)
}

// --- ChannelView -------------------------------------------------------------------------------

func channelViewWireAll() []map[string]any {
	corpus := []struct{ name, doc string }{
		{"zero", `{}`},
		{"full", `{"channel_id":"qr6kf7ztp7yifxt4wm5xn51bke","prev_channel_id":"6bdz674pgq767e4jx75w4pf57a","collapsed_threads_supported":true}`},
		{"channel_only", `{"channel_id":"qr6kf7ztp7yifxt4wm5xn51bke"}`},
		// The documented way to say "I am leaving a channel and entering none".
		{"empty_prev", `{"channel_id":"qr6kf7ztp7yifxt4wm5xn51bke","prev_channel_id":""}`},
		{"crt_false", `{"collapsed_threads_supported":false}`},
		{"explicit_nulls", `{"channel_id":null,"prev_channel_id":null,"collapsed_threads_supported":null}`},
		{"unknown_key", `{"nope":1}`},
		// Go matches field names case-insensitively; we do not ([D-040]).
		{"uppercase_key", `{"CHANNEL_ID":"qr6kf7ztp7yifxt4wm5xn51bke"}`},
		// Neither field is validated, so anything decodes.
		{"not_an_id", `{"channel_id":"nope"}`},
		{"escaped", `{"channel_id":"<a>&b"}`},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			var out model.ChannelView
			err := json.Unmarshal([]byte(c.doc), &out)
			row["ok"] = err == nil
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}
			row["json"] = mustMarshal(&out)
			row["channel_id"] = out.ChannelId
			row["prev_channel_id"] = out.PrevChannelId
			row["collapsed_threads_supported"] = out.CollapsedThreadsSupported
		})
		res = append(res, row)
	}
	return res
}

// --- ChannelViewResponse -------------------------------------------------------------------------

func channelViewResponseWireAll() []map[string]any {
	corpus := []struct{ name, doc string }{
		{"zero", `{}`},
		{"status_only", `{"status":"OK"}`},
		// The distinction the field has no omitempty to hide: nil versus empty.
		{"map_null", `{"status":"OK","last_viewed_at_times":null}`},
		{"map_empty", `{"status":"OK","last_viewed_at_times":{}}`},
		{"one_entry", `{"status":"OK","last_viewed_at_times":{"qr6kf7ztp7yifxt4wm5xn51bke":1700000000000}}`},
		// Unsorted input, because Go sorts on the way out.
		{"unsorted_keys", `{"status":"OK","last_viewed_at_times":{"z":3,"a":1,"m":2}}`},
		{"zero_value", `{"last_viewed_at_times":{"a":0}}`},
		{"negative_value", `{"last_viewed_at_times":{"a":-1}}`},
		{"int64_bounds", `{"last_viewed_at_times":{"lo":-9223372036854775808,"hi":9223372036854775807}}`},
		{"empty_key", `{"last_viewed_at_times":{"":1}}`},
		{"escaped_key", `{"last_viewed_at_times":{"<a>&":1}}`},
		// A duplicate key: the last value wins.
		{"duplicate_key", `{"last_viewed_at_times":{"a":1,"a":2}}`},
		{"duplicate_status", `{"status":"first","status":"second"}`},
		{"explicit_null_status", `{"status":null}`},
		{"unknown_key", `{"nope":1}`},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			var out model.ChannelViewResponse
			err := json.Unmarshal([]byte(c.doc), &out)
			row["ok"] = err == nil
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}
			row["json"] = mustMarshal(&out)
			row["status"] = out.Status
			row["map_nil"] = out.LastViewedAtTimes == nil
			row["map_len"] = len(out.LastViewedAtTimes)
		})
		res = append(res, row)
	}
	return res
}

// --- the map's value position ---------------------------------------------------------------------

// channelViewMapValueAll is the half a client can reach and the half `file.go`'s duration corpus
// could not: a value inside a map rather than a struct field. `null` is the interesting one —
// Go accepts it, and whether it leaves a zero entry or no entry at all is not something the
// `encoding/json` documentation says.
func channelViewMapValueAll() []map[string]any {
	values := []struct{ name, raw string }{
		{"integer", `1700000000000`},
		{"zero", `0`},
		{"negative", `-1`},
		{"max_int64", `9223372036854775807`},
		{"min_int64", `-9223372036854775808`},
		{"max_int64_plus_one", `9223372036854775808`},
		{"fractional_but_whole", `1.0`},
		{"fractional", `1.5`},
		{"exponent", `1e9`},
		{"quoted_number", `"1700000000000"`},
		{"null", `null`},
		{"bool", `true`},
		{"object", `{}`},
		{"array", `[]`},
	}

	var res []map[string]any
	for _, v := range values {
		// Two entries, so a failure on the second can be seen not to have removed the first.
		doc := `{"status":"OK","last_viewed_at_times":{"keep":7,"probe":` + v.raw + `}}`
		row := map[string]any{"name": v.name, "raw": v.raw, "in": doc}
		probe(row, func() {
			var out model.ChannelViewResponse
			err := json.Unmarshal([]byte(doc), &out)
			row["ok"] = err == nil
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}
			row["map_nil"] = out.LastViewedAtTimes == nil
			row["map_len"] = len(out.LastViewedAtTimes)
			// The question `null` raises: is the key there at all, and with what value?
			probeValue, present := out.LastViewedAtTimes["probe"]
			row["probe_present"] = present
			row["probe_value"] = probeValue
			row["keep_value"] = out.LastViewedAtTimes["keep"]
			row["json_after"] = mustMarshal(&out)
		})
		res = append(res, row)
	}
	return res
}

// --- the bool ------------------------------------------------------------------------------------

// channelViewBoolAll pins Go's bool decoding, which is stricter than the shapes a hand-written
// client tends to send. Nothing in this type validates, so whatever decodes is what the handler
// acts on.
func channelViewBoolAll() []map[string]any {
	values := []struct{ name, raw string }{
		{"true", `true`},
		{"false", `false`},
		{"null", `null`},
		{"quoted_true", `"true"`},
		{"one", `1`},
		{"zero", `0`},
		{"empty_string", `""`},
	}

	var res []map[string]any
	for _, v := range values {
		doc := `{"channel_id":"c1","collapsed_threads_supported":` + v.raw + `}`
		row := map[string]any{"name": v.name, "raw": v.raw, "in": doc}
		probe(row, func() {
			var out model.ChannelView
			err := json.Unmarshal([]byte(doc), &out)
			row["ok"] = err == nil
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}
			row["value"] = out.CollapsedThreadsSupported
			row["channel_id_after"] = out.ChannelId
		})
		res = append(res, row)
	}
	return res
}

// --- map marshalling ------------------------------------------------------------------------------

// channelViewMapMarshalAll builds the map in Go rather than decoding it, so the output ordering
// is Go's own and not an echo of the input document. Go sorts map keys by byte value when
// marshalling; a Rust `HashMap` would not, which is the whole of [D-027]'s ordering half.
func channelViewMapMarshalAll() []map[string]any {
	corpus := []struct {
		name string
		in   map[string]int64
	}{
		{"nil", nil},
		{"empty", map[string]int64{}},
		{"one", map[string]int64{"a": 1}},
		{"sorted_input", map[string]int64{"a": 1, "b": 2, "c": 3}},
		{"unsorted_input", map[string]int64{"c": 3, "a": 1, "b": 2}},
		// Byte-value ordering, not lexicographic-by-locale: uppercase sorts before lowercase.
		{"mixed_case", map[string]int64{"b": 2, "A": 1, "a": 3, "B": 4}},
		{"empty_key", map[string]int64{"": 1, "a": 2}},
		{"escaped_keys", map[string]int64{"<a>": 1, "&b": 2, "c ": 3}},
		{"non_ascii_keys", map[string]int64{"é": 1, "z": 2, "日": 3}},
		{"bounds", map[string]int64{"lo": -9223372036854775808, "hi": 9223372036854775807}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			row["map_json"] = mustMarshal(c.in)
			row["nil"] = c.in == nil
			row["in_struct"] = mustMarshal(model.ChannelViewResponse{Status: "OK", LastViewedAtTimes: c.in})
		})
		res = append(res, row)
	}
	return res
}
