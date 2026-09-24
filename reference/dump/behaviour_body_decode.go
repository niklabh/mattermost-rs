package main

// Behavioural oracle for the request-body decoders in mm_model::utils (D-941), written to
// fixtures/behaviour_body_decode.json.
//
// Every api4 handler reads its body one of three ways, and the Rust port has one decoder per way:
//
//   - `var x model.T; json.NewDecoder(r.Body).Decode(&x)`  -> decode_one_value_from_json
//   - `var x *model.T; json.NewDecoder(r.Body).Decode(&x)` -> decode_one_from_json::<Option<T>>
//   - `json.Unmarshal(body, &x)`                           -> unmarshal_from_json
//
// The corpus is aimed at the four places serde and encoding/json part: a JSON array for a struct
// (at the top and nested), `null` for a value versus a pointer, bytes after the first value, and
// a repeated key. The shape is a local struct rather than a model type so that every field kind a
// body holds — scalar, nested struct, pointer to struct, slice of structs, slice of strings,
// map[string]any — is in one document; the Rust test declares the same struct.
//
// Deliberately **not** in the corpus: an explicit `null` for a scalar or inside a slice of
// structs ([D-057], [D-075]) and a key in the wrong case ([D-460]). Each is its own entry, and a
// row here would assert a divergence this oracle is not about.
//
// Determinism: a fixed corpus, no rand, no time.Now.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/mattermost/mattermost/server/public/model"
)

type bodyDecodeInner struct {
	A string `json:"a"`
	N int64  `json:"n"`
}

type bodyDecodeOuter struct {
	Name  string            `json:"name"`
	Count int64             `json:"count"`
	Flag  bool              `json:"flag"`
	Inner bodyDecodeInner   `json:"inner"`
	Maybe *bodyDecodeInner  `json:"maybe"`
	List  []bodyDecodeInner `json:"list"`
	Tags  []string          `json:"tags"`
	Props map[string]any    `json:"props"`
}

var bodyDecodeCorpus = []string{
	// Not an object.
	``, `   `, `{`, `null`, ` null `, `[]`, `["x"]`, `[{}]`, `"s"`, `7`, `true`,
	// Objects, and what follows them.
	`{}`, `{"name":"a"}`, `{"name":"a"} `, "{\"name\":\"a\"}\n", `{"name":"a"} trailing`,
	`{"name":"a"}{"name":"b"}`, `{"name":"a"}[]`, `null x`, `[] x`,
	// Nested shapes.
	`{"inner":{"a":"x","n":3}}`, `{"inner":[]}`, `{"inner":["x"]}`, `{"inner":"x"}`,
	`{"maybe":null}`, `{"maybe":{"a":"m"}}`, `{"maybe":[]}`,
	`{"list":null}`, `{"list":[]}`, `{"list":[{"a":"1"},{"a":"2","n":2}]}`, `{"list":[[]]}`,
	`{"list":{}}`,
	`{"tags":[]}`, `{"tags":["b","a","b"]}`, `{"tags":"a"}`,
	`{"props":{"k":[1,{"x":2}],"s":"v"}}`, `{"props":[]}`, `{"props":null}`,
	// Repeated keys.
	`{"name":"a","name":"b"}`, `{"inner":{"a":"x","a":"y"}}`, `{"name":"a","count":1,"name":"c"}`,
	// Mistyped members.
	`{"count":"7"}`, `{"count":1.5}`, `{"flag":1}`, `{"name":7}`,
	// Unknown keys, and a lone surrogate.
	`{"zzz":1,"name":"a"}`, `{"name":"\ud800"}`,
	// A full document.
	`{"name":"n","count":9,"flag":true,"inner":{"a":"i","n":1},"maybe":{"a":"m","n":2},` +
		`"list":[{"a":"l"}],"tags":["t"],"props":{"p":true}}`,
}

// A row per corpus body: each of the three decodes' success and, when it succeeded, its value.
func bodyDecodeCases() []map[string]any {
	var out []map[string]any
	for _, body := range bodyDecodeCorpus {
		var value bodyDecodeOuter
		valueErr := json.NewDecoder(strings.NewReader(body)).Decode(&value)

		var pointer *bodyDecodeOuter
		pointerErr := json.NewDecoder(strings.NewReader(body)).Decode(&pointer)

		var whole *bodyDecodeOuter
		wholeErr := json.Unmarshal([]byte(body), &whole)

		row := map[string]any{
			"in":         body,
			"value_ok":   valueErr == nil,
			"pointer_ok": pointerErr == nil,
			"whole_ok":   wholeErr == nil,
		}
		if valueErr == nil {
			row["value"] = value
		}
		if pointerErr == nil {
			// nil marshals as `null`, which is the Rust side's `None`.
			row["pointer"] = pointer
		}
		if wholeErr == nil {
			row["whole"] = whole
		}
		out = append(out, row)
	}
	return out
}

// `model.MapBoolFromJSON` — the one map helper `mm_model::utils` adds that the user-update
// oracle's `map_decode` rows do not already cover.
func mapBoolDecodeCases() []map[string]any {
	corpus := []string{
		``, `null`, `[]`, `"x"`, `{`, `{}`,
		`{"collapsed_threads_supported":true}`,
		`{"collapsed_threads_supported":true,"x":"nope"}`,
		`{"x":"nope","collapsed_threads_supported":true}`,
		`{"a":1}`, `{"a":null}`, `{"a":"true"}`, `{"a":false,"a":true}`,
		`{"a":true} trailing`, `{"a":{"b":true}}`,
	}
	var out []map[string]any
	for _, body := range corpus {
		out = append(out, map[string]any{
			"in":  body,
			"out": model.MapBoolFromJSON(strings.NewReader(body)),
		})
	}
	return out
}

// `var n int64; Decode(&n)` — `serveSortOrder`'s body (views.go), the one scalar value target.
func int64DecodeCases() []map[string]any {
	corpus := []string{`2`, `0`, `-1`, `null`, `1.5`, `"2"`, `[]`, `{}`, `2 x`, ``}
	var out []map[string]any
	for _, body := range corpus {
		var n int64
		err := json.NewDecoder(strings.NewReader(body)).Decode(&n)
		row := map[string]any{"in": body, "ok": err == nil}
		if err == nil {
			row["out"] = n
		}
		out = append(out, row)
	}
	return out
}

func writeBodyDecodeBehaviourFixture(outDir string) error {
	out := map[string]any{
		"struct_decode":   bodyDecodeCases(),
		"map_bool_decode": mapBoolDecodeCases(),
		"int64_decode":    int64DecodeCases(),
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_body_decode.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}
