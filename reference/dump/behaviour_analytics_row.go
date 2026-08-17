package main

// Behavioural oracle for model/analytics_row.go, written to fixtures/behaviour_analytics_row.json.
//
// Eleven lines: `{Name string; Value float64}` and `type AnalyticsRows []*AnalyticsRow`. No
// methods, no tags worth arguing about — and the **first `float64` on the wire in the ported
// tree**, which is the whole reason this file gets an oracle instead of a round-trip test.
//
// # `encoding/json` renders a float three ways and none of them is `%v`
//
// The crate already has `utils::go_format_float`, which is Go's `%g` — `strconv.FormatFloat(f,
// 'g', -1, 64)`, exponent form outside `[1e-4, 1e6)`. **That is not the JSON rendering.**
// `encoding/json`'s float encoder is:
//
//	fmt := byte('f')
//	if abs := math.Abs(f); abs != 0 && (abs < 1e-6 || abs >= 1e21) { fmt = 'e' }
//	b = strconv.AppendFloat(b, f, fmt, -1, 64)
//	if fmt == 'e' { /* rewrite a trailing "e-09" to "e-9" */ }
//
// So the thresholds are 1e-6 and 1e21 rather than 1e-4 and 1e6, and the exponent has its leading
// zero **stripped** — `1e-07` becomes `1e-7`, which is not what any of Go's own `strconv` modes
// emits and not what Rust or serde_json emits either. Three renderings of the same value, and the
// corpus records all three side by side so a port cannot reach for the wrong one silently.
//
// # NaN and the infinities are an error, not a null
//
// `json.Marshal` returns `json: unsupported value: NaN` and produces **no output at all**. So a
// single bad analytics row fails the entire response rather than degrading one field — which is
// the opposite of what a port that mapped them to `null` would do.
//
// # An integral float has no decimal point
//
// Go writes `1`, not `1.0`. serde_json writes `1.0`. That is a divergence on the most ordinary
// value this type will ever carry — an analytics count — and it is why the corpus leads with the
// small integers rather than the exotic values.
//
// Determinism: fixed values only. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"math"
	"os"
	"path/filepath"
	"strconv"

	"github.com/mattermost/mattermost/server/public/model"
)

func writeAnalyticsRowBehaviourFixture(outDir string) error {
	out := map[string]any{
		"float_wire":   analyticsFloatWireAll(),
		"float_decode": analyticsFloatDecodeAll(),
		"row_wire":     analyticsRowWireAll(),
		"rows_wire":    analyticsRowsWireAll(),
		"unsupported":  analyticsUnsupportedAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_analytics_row.json"), append(blob, '\n'), 0o644)
}

// --- the float rendering ---------------------------------------------------------------------------

// analyticsFloatCorpus is the shared value set. It is built to straddle every threshold in the
// encoder rather than to look like analytics data.
func analyticsFloatCorpus() []struct {
	name string
	in   float64
} {
	return []struct {
		name string
		in   float64
	}{
		// The ordinary case, and the one a port gets wrong first: no decimal point.
		{"zero", 0},
		{"negative_zero", math.Copysign(0, -1)},
		{"one", 1},
		{"negative_one", -1},
		{"integral_large", 1234567},
		{"count_like", 42},

		// Fractions.
		{"half", 0.5},
		{"tenth", 0.1},
		{"third", 1.0 / 3.0},
		{"negative_fraction", -2.75},

		// The lower threshold: 'f' below 1e-6 becomes 'e' at and under it.
		{"just_above_lower_threshold", 1.000001e-6},
		{"at_lower_threshold", 1e-6},
		{"just_below_lower_threshold", 9.99999e-7},
		{"one_e_minus_7", 1e-7},
		{"one_e_minus_9", 1e-9},
		{"one_e_minus_10", 1e-10},
		{"smallest_normal", 2.2250738585072014e-308},
		{"smallest_subnormal", 5e-324},

		// The upper threshold: 'f' below 1e21, 'e' at and above.
		{"just_below_upper_threshold", 9.999999999999999e20},
		{"at_upper_threshold", 1e21},
		{"just_above_upper_threshold", 1.0000000000000001e21},
		{"one_e_22", 1e22},
		{"max_float64", math.MaxFloat64},

		// Integers that no longer round-trip through a float64.
		{"two_pow_53", 9007199254740992},
		{"two_pow_53_plus_two", 9007199254740994},
		{"max_safe_integer", 9007199254740991},

		// Digit strings that stress the shortest-representation algorithm.
		{"pi", math.Pi},
		{"many_digits", 1.2345678901234567},
		{"trailing_nines", 0.30000000000000004},
	}
}

// analyticsFloatWireAll records, for each value, all three renderings Go can produce plus the
// document `AnalyticsRow` actually emits. `fmt_v` and `strconv_g` are NOT the wire form — they are
// recorded precisely so a port that reaches for `utils::go_format_float` fails a test that names
// the difference.
func analyticsFloatWireAll() []map[string]any {
	var res []map[string]any
	for _, c := range analyticsFloatCorpus() {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			// The wire form: what encoding/json writes for the bare value.
			row["json"] = mustMarshal(c.in)
			// ...and inside the struct, which is what a client receives.
			row["in_row"] = mustMarshal(model.AnalyticsRow{Name: c.name, Value: c.in})

			// The two renderings that are not the wire form.
			row["fmt_v"] = strconv.FormatFloat(c.in, 'g', -1, 64)
			row["strconv_f"] = strconv.FormatFloat(c.in, 'f', -1, 64)

			// The bits, so the Rust side reconstructs the exact same float rather than parsing a
			// decimal literal and hoping.
			row["bits"] = math.Float64bits(c.in)
		})
		res = append(res, row)
	}
	return res
}

// analyticsFloatDecodeAll is the inbound half. A JSON number decodes into a float64 with no
// integer rules at all, so everything the map-value corpus in behaviour_channel_view.go rejects is
// accepted here — `1.5`, `1e9`, and an integer far past what a float can represent.
func analyticsFloatDecodeAll() []map[string]any {
	values := []struct{ name, raw string }{
		{"integer", `1`},
		{"integer_with_point", `1.0`},
		{"fractional", `1.5`},
		{"exponent", `1e9`},
		{"negative_exponent", `1e-9`},
		{"capital_exponent", `1E9`},
		{"explicit_plus_exponent", `1e+9`},
		{"negative", `-2.75`},
		{"negative_zero", `-0`},
		// Past float64's precision: accepted, and silently rounded.
		{"beyond_precision", `9007199254740993`},
		// Past float64's range: this is where Go draws the line.
		{"overflow", `1e400`},
		{"underflow", `1e-400`},
		{"max_float64_literal", `1.7976931348623157e308`},
		// Not numbers.
		{"quoted", `"1.5"`},
		{"null", `null`},
		{"bool", `true`},
		// JSON does not allow these spellings, and neither does Go.
		{"nan_literal", `NaN`},
		{"infinity_literal", `Infinity`},
		{"leading_plus", `+1`},
		{"hex", `0x10`},
	}

	var res []map[string]any
	for _, v := range values {
		doc := `{"name":"n","value":` + v.raw + `}`
		row := map[string]any{"name": v.name, "raw": v.raw, "in": doc}
		probe(row, func() {
			var out model.AnalyticsRow
			err := json.Unmarshal([]byte(doc), &out)
			row["ok"] = err == nil
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}
			row["value_bits"] = math.Float64bits(out.Value)
			row["name_after"] = out.Name
			// Only meaningful when ok; recorded regardless so a partial decode is visible.
			if !math.IsNaN(out.Value) && !math.IsInf(out.Value, 0) {
				row["json_after"] = mustMarshal(out)
			}
		})
		res = append(res, row)
	}
	return res
}

// --- the struct and the slice -----------------------------------------------------------------------

func analyticsRowWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.AnalyticsRow
	}{
		{"zero", model.AnalyticsRow{}},
		{"typical", model.AnalyticsRow{Name: "total_posts", Value: 1234}},
		{"fractional", model.AnalyticsRow{Name: "avg_per_day", Value: 12.5}},
		{"negative", model.AnalyticsRow{Name: "delta", Value: -3}},
		{"empty_name", model.AnalyticsRow{Name: "", Value: 1}},
		{"escaped_name", model.AnalyticsRow{Name: "<a>&b", Value: 1}},
		{"exponent_value", model.AnalyticsRow{Name: "tiny", Value: 1e-9}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			row["json"] = mustMarshal(c.in)
			row["value_bits"] = math.Float64bits(c.in.Value)
		})
		res = append(res, row)
	}
	return res
}

// analyticsRowsWireAll drives the slice alias. It is `[]*AnalyticsRow`, so nil, empty and a nil
// element are three distinct documents — and the last of them is one we cannot decode ([D-033]).
func analyticsRowsWireAll() []map[string]any {
	corpus := []struct {
		name string
		in   model.AnalyticsRows
	}{
		{"nil", nil},
		{"empty", model.AnalyticsRows{}},
		{"one", model.AnalyticsRows{{Name: "a", Value: 1}}},
		{"several", model.AnalyticsRows{{Name: "a", Value: 1}, {Name: "b", Value: 2.5}, {Name: "c", Value: -3}}},
		{"nil_element", model.AnalyticsRows{{Name: "a", Value: 1}, nil}},
		{"only_a_nil_element", model.AnalyticsRows{nil}},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name}
		probe(row, func() {
			row["json"] = mustMarshal(c.in)
			row["nil"] = c.in == nil
			row["len"] = len(c.in)
			nils := []bool{}
			for _, r := range c.in {
				nils = append(nils, r == nil)
			}
			row["element_nil"] = nils
		})
		res = append(res, row)
	}
	return res
}

// --- the values Go refuses to marshal ----------------------------------------------------------------

// analyticsUnsupportedAll is the one place this file can fail. `json.Marshal` returns an error and
// **no output** for NaN and the infinities, so one bad row aborts the whole response — it does not
// degrade to null. Recorded with the exact error text, because that string is what a Go handler
// logs and what a port's error type should be able to reproduce.
func analyticsUnsupportedAll() []map[string]any {
	values := []struct {
		name string
		in   float64
	}{
		{"nan", math.NaN()},
		{"positive_infinity", math.Inf(1)},
		{"negative_infinity", math.Inf(-1)},
	}

	var res []map[string]any
	for _, v := range values {
		row := map[string]any{"name": v.name, "bits": math.Float64bits(v.in)}
		probe(row, func() {
			// The bare value.
			blob, err := json.Marshal(v.in)
			row["ok"] = err == nil
			row["output"] = string(blob)
			if err != nil {
				row["err"] = err.Error()
			} else {
				row["err"] = nil
			}

			// Inside the struct, and inside the slice: the failure propagates all the way up, so
			// a single bad row loses every good one alongside it.
			_, rowErr := json.Marshal(model.AnalyticsRow{Name: "n", Value: v.in})
			row["row_err"] = errText(rowErr)

			_, sliceErr := json.Marshal(model.AnalyticsRows{{Name: "good", Value: 1}, {Name: "bad", Value: v.in}})
			row["slice_err"] = errText(sliceErr)

			// And what fmt does instead, which is where the "+Inf"/"NaN" spellings come from —
			// they are Go's %v, not anything JSON ever writes.
			row["fmt_v"] = strconv.FormatFloat(v.in, 'g', -1, 64)
		})
		res = append(res, row)
	}
	return res
}

func errText(err error) any {
	if err == nil {
		return nil
	}
	return err.Error()
}
