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
// The corpus also drives the rules the adapter took on after D-941: `null` into a scalar, a struct,
// a slice element and a map value (D-057, D-075), keys matched case-insensitively with Go's
// foldName — including U+212A KELVIN SIGN and U+017F LONG S, the two non-ASCII runes whose fold
// is ASCII — (D-040, D-460), and a repeated key or a repeated folded spelling, which Go assigns
// again in document order with a per-kind meaning of "again" (D-071).
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

// A struct Go embeds another in (D-1240): the embedded fields are promoted, so every rule — the
// fold, null, a repeated key, a nested null — reaches them exactly as it reaches the outer's own.
type bodyDecodeEmbedBase struct {
	A     string          `json:"a"`
	Tags  []string        `json:"tags"`
	Inner bodyDecodeInner `json:"inner"`
}

type bodyDecodeEmbed struct {
	bodyDecodeEmbedBase
	N     int64    `json:"n"`
	Items []string `json:"items"`
}

var bodyDecodeEmbedCorpus = []string{
	`{}`, `null`, `[]`, `{"a":"x","n":1}`, `{"A":"x","N":1}`, `{"a":null,"n":null}`,
	`{"tags":[null,"t"],"items":[null]}`, `{"inner":{"A":"i","n":null}}`,
	`{"a":"x","A":"y","n":1,"n":2}`, `{"inner":{"a":"x"},"inner":{"n":3}}`,
	`{"tags":["a","b"],"tags":[null]}`, `{"TAGS":["t"],"ITEMS":["i"]}`, `{"inner":[]}`,
	`{"a":7}`, `{"n":"7"}`, "{\"\u017ftate\":1,\"a\":\"s\"}",
}

func bodyDecodeEmbedCases() []map[string]any {
	var out []map[string]any
	for _, body := range bodyDecodeEmbedCorpus {
		var value bodyDecodeEmbed
		err := json.NewDecoder(strings.NewReader(body)).Decode(&value)
		row := map[string]any{"in": body, "ok": err == nil}
		if err == nil {
			row["value"] = value
		}
		out = append(out, row)
	}
	return out
}

// Two tags that differ only by case: the exact spelling finds its own field, and a folded one
// finds the first declared (encode.go:1306, "first folded match takes precedence").
type bodyDecodeCollide struct {
	Lower string `json:"k"`
	Upper string `json:"K"`
}

var bodyDecodeCollideCorpus = []string{
	`{"k":"l"}`, `{"K":"u"}`, "{\"\u212a\":\"kelvin\"}", `{"k":"l","K":"u"}`, "{\"K\":\"u\",\"\u212a\":\"kelvin\"}",
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
	// null into every kind (D-057, D-075).
	`{"name":null,"count":null,"flag":null}`, `{"inner":null}`, `{"inner":{"a":null,"n":null}}`,
	`{"tags":[null]}`, `{"tags":["a",null,"b"]}`, `{"list":[null]}`, `{"list":[null,{"a":"x"}]}`,
	`{"props":{"k":null}}`, `{"maybe":{"a":null}}`,
	// Keys folded (D-040, D-460).
	`{"NAME":"a"}`, `{"Name":"a","COUNT":3,"Flag":true}`, `{"INNER":{"A":"x","N":1}}`,
	`{"nAmE":"a"}`, `{"na_me":"a"}`, `{"TAGS":["t"]}`, `{"PROPS":{"K":1}}`,
	"{\"TAG\u017f\":[\"s\"]}", "{\"inner\":{\"\u212a\":1}}", "{\"n\u0430me\":\"cyrillic\"}",
	// Repeated keys and repeated spellings (D-071).
	`{"name":"a","NAME":"b"}`, `{"NAME":"b","name":"a"}`, `{"name":"a","name":null}`,
	`{"count":1,"count":null}`, `{"name":7,"name":"a"}`, `{"name":"a","name":7}`,
	`{"inner":{"a":"x"},"inner":{"n":2}}`, `{"inner":{"a":"x"},"inner":null}`,
	`{"inner":{"a":"x"},"INNER":{"n":2}}`,
	`{"maybe":{"a":"x"},"maybe":{"n":2}}`, `{"maybe":{"a":"x"},"maybe":null,"maybe":{"n":2}}`,
	`{"tags":["a","b"],"tags":["x"]}`, `{"tags":["a","b"],"tags":[null]}`,
	`{"tags":["a","b"],"tags":["x"],"tags":[null,null]}`, `{"tags":["a","b"],"tags":[],"tags":[null,null]}`,
	`{"tags":["a","b"],"tags":null,"tags":[null]}`,
	`{"list":[{"a":"x"}],"list":[{"n":1}]}`,
	`{"props":{"a":1,"b":2},"props":{"a":3}}`, `{"props":{"a":1},"props":null,"props":{"b":2}}`,
	`{"props":{"a":{"x":1}},"props":{"a":{"y":2}}}`,
	`{"inner":[],"inner":{}}`, `{"tags":"a","tags":[]}`,
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

// The collision struct, through Decode into a value.
func bodyDecodeCollideCases() []map[string]any {
	var out []map[string]any
	for _, body := range bodyDecodeCollideCorpus {
		var value bodyDecodeCollide
		err := json.NewDecoder(strings.NewReader(body)).Decode(&value)
		row := map[string]any{"in": body, "ok": err == nil}
		if err == nil {
			row["value"] = value
		}
		out = append(out, row)
	}
	return out
}

func writeBodyDecodeBehaviourFixture(outDir string) error {
	out := map[string]any{
		"collide_decode":  bodyDecodeCollideCases(),
		"embed_decode":    bodyDecodeEmbedCases(),
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
