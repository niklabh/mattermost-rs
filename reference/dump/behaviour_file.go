package main

// Behavioural oracle for model/file.go, written to fixtures/behaviour_file.json.
//
// Twenty lines: one constant and two response structs with no methods. Nineteen of those lines
// are unremarkable and the twentieth is not:
//
//	type PresignURLResponse struct {
//	    URL        string        `json:"url"`
//	    Expiration time.Duration `json:"expiration"`
//	}
//
// **`time.Duration` has no `MarshalJSON`.** It is a defined `int64`, and `encoding/json` treats
// it as one — so the wire value is a bare integer of **nanoseconds**, not `"1h0m0s"` and not
// seconds. `Duration.String()` exists and produces `1h0m0s`, which is what makes this worth
// measuring rather than reasoning about: the type has a human-readable rendering that the JSON
// encoder never reaches for, and the obvious Rust choices go wrong in both directions —
// `std::time::Duration` serialises as a `{secs, nanos}` object, and `chrono::TimeDelta` has no
// serde impl at all.
//
// The decode side is the sharper half, because it is where a client's input lands. `int64` is
// the target type, so `encoding/json` applies its integer rules: a fractional number is an
// error even when it is exactly representable, a quoted number is an error, and an out-of-range
// number is an error that leaves the field at zero while *continuing* the decode. The corpus
// records the error string for each, because "which of these does Go 400 on" is the question a
// port has to answer identically.
//
// The rest is the ordinary nillable-slice shape: neither field of `FileUploadResponse` carries
// `omitempty`, so nil and empty are both on the wire and differ (`null` versus `[]`).
//
// Determinism: fixed documents and fixed constants only. No `NewId`, no `time.Now` — see
// [D-032]. The `filInfo*` constants are shared with behaviour_file_info_list.go.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"time"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeFileBehaviourFixture(outDir string) error {
	out := map[string]any{
		"constants":          fileConstants(),
		"duration_marshal":   fileDurationMarshalAll(),
		"duration_unmarshal": fileDurationUnmarshalAll(),
		"upload_wire":        fileUploadWireAll(),
		"presign_wire":       filePresignWireAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_file.json"), append(blob, '\n'), 0o644)
}

// --- constants -------------------------------------------------------------------------------

// MaxImageSize is written `int64(6048 * 4032)` in the source — an explicit conversion, so it is a
// typed constant rather than an untyped one, and the product is computed at compile time. Both
// the value and the fact that it fits an int64 are recorded.
func fileConstants() map[string]any {
	return map[string]any{
		"MaxImageSize": model.MaxImageSize,
		// The factors, so a Rust port can be written as the product rather than as a literal and
		// still be checked.
		"MaxImageSizeWidth":  6048,
		"MaxImageSizeHeight": 4032,
	}
}

// --- time.Duration on the wire ------------------------------------------------------------------

// fileDurationMarshalAll pins that Go emits the raw nanosecond count. `string` records what
// `Duration.String()` would have produced for the same value — it is NOT what goes on the wire,
// and it is recorded precisely so a port that reaches for the human-readable form fails a test
// that says why.
func fileDurationMarshalAll() []map[string]any {
	corpus := []struct {
		name string
		in   time.Duration
	}{
		{"zero", 0},
		{"one_nanosecond", 1},
		{"one_microsecond", time.Microsecond},
		{"one_millisecond", time.Millisecond},
		{"one_second", time.Second},
		{"one_minute", time.Minute},
		{"one_hour", time.Hour},
		{"twenty_four_hours", 24 * time.Hour},
		{"negative_one_nanosecond", -1},
		{"negative_hour", -time.Hour},
		// A value that is not a whole number of any unit, so a port that divides loses it.
		{"awkward", 90*time.Minute + 500*time.Millisecond + 7},
		{"max_int64", time.Duration(1<<63 - 1)},
		{"min_int64", time.Duration(-1 << 63)},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			row["nanoseconds"] = int64(c.in)
			row["json"] = mustMarshal(c.in)
			row["in_struct"] = mustMarshal(model.PresignURLResponse{URL: "u", Expiration: c.in})
			// Deliberately not the wire form. See the function comment.
			row["string"] = c.in.String()
		})
		res = append(res, row)
	}
	return res
}

// fileDurationUnmarshalAll is the half a client can reach. Each case is the raw JSON value for
// `expiration`; the row records whether Go accepted it, what the field held afterwards, and the
// error text when it did not.
//
// `url` is set in every document so a decode that fails on `expiration` can still be seen to have
// populated the field before it — Go's decoder does not abandon the object on a range error.
func fileDurationUnmarshalAll() []map[string]any {
	values := []struct{ name, raw string }{
		{"zero", `0`},
		{"one", `1`},
		{"hour_in_nanoseconds", `3600000000000`},
		{"negative", `-1`},
		{"max_int64", `9223372036854775807`},
		{"min_int64", `-9223372036854775808`},
		// One past the top: an out-of-range integer.
		{"max_int64_plus_one", `9223372036854775808`},
		// Exactly representable as a float, and still not an integer literal.
		{"fractional_but_whole", `1.0`},
		{"fractional", `1.5`},
		{"exponent", `1e9`},
		{"exponent_huge", `1e19`},
		// The human-readable form Duration.String() produces, which is the shape a hand-written
		// client is most likely to send.
		{"duration_string", `"1h"`},
		{"quoted_number", `"3600000000000"`},
		{"null", `null`},
		{"bool", `true`},
		{"object", `{}`},
		{"array", `[]`},
	}

	var res []map[string]any
	for _, v := range values {
		doc := `{"url":"https://example.com/f","expiration":` + v.raw + `}`
		row := map[string]any{"name": v.name, "raw": v.raw, "in": doc}
		probe(row, func() {
			var out model.PresignURLResponse
			err := json.Unmarshal([]byte(doc), &out)
			if err != nil {
				row["ok"] = false
				row["err"] = err.Error()
			} else {
				row["ok"] = true
				row["err"] = nil
			}
			// Recorded either way: a range error still leaves the earlier fields populated.
			row["url_after"] = out.URL
			row["expiration_after"] = int64(out.Expiration)
			row["json_after"] = mustMarshal(out)
		})
		res = append(res, row)
	}
	return res
}

// --- FileUploadResponse ---------------------------------------------------------------------

// fileUploadWireAll drives the nillable slices. Neither field has omitempty, so all four
// combinations of nil and empty are distinguishable on the wire.
func fileUploadWireAll() []map[string]any {
	corpus := []struct{ name, doc string }{
		{"zero", `{}`},
		{"explicit_nulls", `{"file_infos":null,"client_ids":null}`},
		{"empty", `{"file_infos":[],"client_ids":[]}`},
		{"infos_only", `{"file_infos":[` + filInfo1 + `]}`},
		{"ids_only", `{"client_ids":["c-1","c-2"]}`},
		{"both", `{"file_infos":[` + filInfo1 + `,` + filInfo2 + `],"client_ids":["c-1","c-2"]}`},
		{"empty_id", `{"client_ids":[""]}`},
		{"duplicate_ids", `{"client_ids":["c-1","c-1"]}`},
		// Go's []*FileInfo accepts a nil element and ours cannot ([D-033]).
		{"nil_info", `{"file_infos":[null],"client_ids":["c-1"]}`},
		{"nil_info_among_real", `{"file_infos":[` + filInfo1 + `,null],"client_ids":[]}`},
		{"unknown_key", `{"nope":1}`},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			var out model.FileUploadResponse
			if err := json.Unmarshal([]byte(c.doc), &out); err != nil {
				panic(err)
			}
			row["json"] = mustMarshal(&out)
			row["infos_nil"] = out.FileInfos == nil
			row["ids_nil"] = out.ClientIds == nil
			row["infos_len"] = len(out.FileInfos)
			// Which elements came back nil, so the [D-033] cases are legible.
			nils := []bool{}
			for _, fi := range out.FileInfos {
				nils = append(nils, fi == nil)
			}
			row["info_element_nil"] = nils
		})
		res = append(res, row)
	}
	return res
}

// --- PresignURLResponse -------------------------------------------------------------------------

// filePresignWireAll is the whole-struct round trip. Neither field has omitempty and neither is a
// pointer, so the zero value is two keys rather than `{}`.
func filePresignWireAll() []map[string]any {
	corpus := []struct{ name, doc string }{
		{"zero", `{}`},
		{"url_only", `{"url":"https://example.com/f"}`},
		{"expiration_only", `{"expiration":3600000000000}`},
		{"both", `{"url":"https://example.com/f?x=1","expiration":900000000000}`},
		{"empty_url", `{"url":"","expiration":0}`},
		// The field name is `URL` with tag `url`; Go also matches `URL` case-insensitively.
		{"uppercase_key", `{"URL":"https://example.com/f"}`},
		{"escaped_url", `{"url":"https://example.com/f?a=1&b=2<x>"}`},
		{"unknown_key", `{"nope":1}`},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name, "in": c.doc}
		probe(row, func() {
			var out model.PresignURLResponse
			if err := json.Unmarshal([]byte(c.doc), &out); err != nil {
				panic(err)
			}
			row["json"] = mustMarshal(&out)
			row["url"] = out.URL
			row["expiration"] = int64(out.Expiration)
		})
		res = append(res, row)
	}
	return res
}
