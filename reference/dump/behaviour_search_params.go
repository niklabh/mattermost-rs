package main

// Behavioural oracle for model/search_params.go, written to
// fixtures/behaviour_search_params.json.
//
// This is the search box: the string a user types becomes a []*SearchParams here. It is 398
// lines of branching text handling and almost none of it is settled by reading it.
//
//  1. **Two of the three functions are unexported.** `splitWords` and `parseSearchFlags` cannot
//     be called from this package, so every case below drives them through `ParseSearchParams`
//     and observes the composition. The corpus is built so that each branch of the two helpers
//     changes the *output* — a quote that never closes, a flag whose value is empty, a `-`
//     immediately before a quote — rather than testing the happy path and hoping.
//
//  2. **Go's `\d` and `\s` are ASCII; Rust's are Unicode.** The two term-trimming regexes are
//     negated classes over `\pL`, `\d` and `\s`, so the difference inverts: a character Go does
//     not consider a digit is one Go *strips*. `regexp_probes` sweeps all 128 ASCII codepoints
//     and 40 hand-picked others through each pattern so the Rust port can be written against
//     measured classes instead of a hope that the crates agree.
//
//  3. **The patterns themselves are read out of the Go source with `go/parser`**, the same
//     treatment behaviour_version.go gives the unexported `versions` table. Transcribing four
//     regexes by hand into a fixture would assert what we already believe.
//
//  4. **`strings.Fields` splits on `unicode.IsSpace`, which is a different set from the `\s` in
//     the regexes two lines away.** Recorded separately, over the same sweep, because the port
//     needs to know that the splitter and the trimmer disagree about U+00A0.
//
//  5. **Two of the six date accessors fall back to `time.Now()`** when the date fails to parse,
//     and the other four return 0. A clock-dependent answer cannot go in a committed fixture, so
//     those cases record `uses_now` and the Rust test recomputes rather than comparing.
//
//  6. **`GetStartOfDayMillis` builds a `time.FixedZone` from an offset in *seconds* and does not
//     range-check it.** chrono's `FixedOffset` stops at ±86399, so the port cannot delegate;
//     `day_millis` measures offsets well past a whole day, both signs.

import (
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"

	"github.com/mattermost/mattermost/server/public/model"
)

const (
	searchParamsGoPath = "../mattermost/server/public/model/search_params.go"
	utilsGoPath        = "../mattermost/server/public/model/utils.go"
)

func writeSearchParamsBehaviourFixture(outDir string) error {
	patterns, err := searchParamsPatterns()
	if err != nil {
		return err
	}

	out := map[string]any{
		"regexps":               patterns,
		"regexp_probes":         spRegexpProbes(patterns),
		"strings_fields":        spStringsFieldsAll(),
		"pad_date_string_zeros": spPadAll(),
		"day_millis":            spDayMillisAll(),
		"date_millis":           spDateMillisAll(),
		"parse_search_params":   spParseAll(),
		"is_valid_list":         spIsValidListAll(),
		"wire":                  spWireAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_search_params.json"), append(blob, '\n'), 0o644)
}

// --- the unexported regexes -------------------------------------------------------------------

// searchParamsPatterns reads the four `regexp.MustCompile` literals the term parser uses straight
// out of the Go source. Two live in search_params.go and two in utils.go, and all four are
// unexported, so the alternative is transcription — which is what this project's fixtures exist
// to avoid.
func searchParamsPatterns() (map[string]string, error) {
	want := map[string][]string{
		searchParamsGoPath: {"searchTermPuncStart", "searchTermPuncEnd"},
		utilsGoPath:        {"validHashtag", "hashtagStart"},
	}

	out := map[string]string{}
	for path, names := range want {
		found, err := parseRegexpLiterals(path, names)
		if err != nil {
			return nil, err
		}
		for name, pattern := range found {
			out[name] = pattern
		}
	}
	for _, names := range want {
		for _, name := range names {
			if out[name] == "" {
				return nil, fmt.Errorf("no regexp literal found for %s", name)
			}
		}
	}
	return out, nil
}

// parseRegexpLiterals finds `var <name> = regexp.MustCompile("<pattern>")` declarations and
// returns the raw pattern strings.
func parseRegexpLiterals(path string, names []string) (map[string]string, error) {
	wanted := map[string]bool{}
	for _, n := range names {
		wanted[n] = true
	}

	fset := token.NewFileSet()
	file, err := parser.ParseFile(fset, path, nil, 0)
	if err != nil {
		return nil, fmt.Errorf("parsing %s: %w", path, err)
	}

	out := map[string]string{}
	for _, decl := range file.Decls {
		gen, ok := decl.(*ast.GenDecl)
		if !ok || gen.Tok != token.VAR {
			continue
		}
		for _, spec := range gen.Specs {
			value, ok := spec.(*ast.ValueSpec)
			if !ok || len(value.Names) != 1 || !wanted[value.Names[0].Name] {
				continue
			}
			if len(value.Values) != 1 {
				return nil, fmt.Errorf("%s has %d initialisers, want 1", value.Names[0].Name, len(value.Values))
			}
			call, ok := value.Values[0].(*ast.CallExpr)
			if !ok || len(call.Args) != 1 {
				return nil, fmt.Errorf("%s is not a one-argument call", value.Names[0].Name)
			}
			basic, ok := call.Args[0].(*ast.BasicLit)
			if !ok || basic.Kind != token.STRING {
				return nil, fmt.Errorf("%s's argument is not a string literal", value.Names[0].Name)
			}
			unquoted, err := strconv.Unquote(basic.Value)
			if err != nil {
				return nil, fmt.Errorf("unquoting %s: %w", basic.Value, err)
			}
			out[value.Names[0].Name] = unquoted
		}
	}
	return out, nil
}

// --- the character classes --------------------------------------------------------------------

// spProbeRunes is every ASCII codepoint plus a hand-picked set chosen to separate Go's ASCII
// `\d`/`\s` from Unicode's: digits that are not [0-9], spaces that are not [\t\n\f\r ], marks,
// letters outside Latin, and the zero-width characters that show up in pasted search text.
func spProbeRunes() []rune {
	runes := make([]rune, 0, 128+40)
	for r := rune(0); r < 128; r++ {
		runes = append(runes, r)
	}
	return append(runes,
		0x0085,  // NEL — unicode.IsSpace, not regexp \s
		0x00A0,  // NBSP — unicode.IsSpace, not regexp \s
		0x00B2,  // ² SUPERSCRIPT TWO — category No, not Nd
		0x00BD,  // ½ — category No
		0x00E9,  // é
		0x0130,  // İ
		0x0301,  // combining acute — category Mn, in \p{M}
		0x0345,  // combining ypogegrammeni — Mn
		0x0374,  // ʹ greek numeral sign — Lm
		0x03A3,  // Σ
		0x0430,  // а cyrillic
		0x05D0,  // א hebrew
		0x0660,  // ٠ ARABIC-INDIC ZERO — category Nd, not [0-9]
		0x0663,  // ٣ ARABIC-INDIC THREE — category Nd, not [0-9]
		0x06F1,  // ۱ EXTENDED ARABIC-INDIC ONE — Nd
		0x0966,  // ० DEVANAGARI ZERO — Nd
		0x09F4,  // ৴ BENGALI CURRENCY NUMERATOR ONE — No
		0x0E50,  // ๐ THAI ZERO — Nd
		0x1680,  // OGHAM SPACE MARK
		0x2000,  // EN QUAD
		0x2007,  // FIGURE SPACE — not unicode.IsSpace
		0x200B,  // ZERO WIDTH SPACE — not unicode.IsSpace
		0x2028,  // LINE SEPARATOR
		0x2029,  // PARAGRAPH SEPARATOR
		0x202F,  // NARROW NO-BREAK SPACE
		0x205F,  // MEDIUM MATHEMATICAL SPACE
		0x2070,  // ⁰ SUPERSCRIPT ZERO — No
		0x2160,  // Ⅰ ROMAN NUMERAL ONE — Nl, not Nd
		0x2167,  // Ⅷ — Nl
		0x2212,  // − MINUS SIGN — Sm, not ASCII '-'
		0x2019,  // ’ RIGHT SINGLE QUOTATION MARK
		0x201C,  // “ LEFT DOUBLE QUOTATION MARK — not the ASCII quote splitWords looks for
		0x2605,  // ★ — So
		0x3000,  // IDEOGRAPHIC SPACE
		0x3007,  // 〇 IDEOGRAPHIC NUMBER ZERO — Nl
		0x30A0,  // ゠ KATAKANA-HIRAGANA DOUBLE HYPHEN — Pd
		0x4E00,  // 一 CJK — Lo
		0xFEFF,  // ZERO WIDTH NO-BREAK SPACE
		0xFF10,  // ０ FULLWIDTH DIGIT ZERO — Nd
		0x1D7CE, // 𝟎 MATHEMATICAL BOLD DIGIT ZERO — Nd, outside the BMP
		0x1F600, // 😀
	)
}

type spRegexpCase struct {
	// Rune is recorded as a codepoint rather than as text so a fixture reader can see exactly
	// which character a row is about, and so control characters survive JSON.
	Rune int    `json:"rune"`
	In   string `json:"in"`
	Out  string `json:"out"`
}

// spRegexpProbes runs each pattern over the sweep. The two term regexes are anchored to one end,
// so each probe wraps the codepoint in a fixed neutral context ("aa") to isolate the class from
// the anchor: what is being measured is whether the character is in the negated set.
func spRegexpProbes(patterns map[string]string) map[string]any {
	start := regexp.MustCompile(patterns["searchTermPuncStart"])
	end := regexp.MustCompile(patterns["searchTermPuncEnd"])
	hashStart := regexp.MustCompile(patterns["hashtagStart"])
	hashValid := regexp.MustCompile(patterns["validHashtag"])

	var startCases, endCases []spRegexpCase
	for _, r := range spProbeRunes() {
		lead := string(r) + "aa"
		startCases = append(startCases, spRegexpCase{int(r), lead, start.ReplaceAllString(lead, "")})
		trail := "aa" + string(r)
		endCases = append(endCases, spRegexpCase{int(r), trail, end.ReplaceAllString(trail, "")})
	}

	// Whole-word corpora, where the anchors and the repetition matter as much as the class.
	words := []string{
		"", "a", "#", "##", "###", "#a", "#ab", "#a-b", "#a_b", "#a.b", "#a1", "#1a", "#ab-",
		"#ab.", "#ab_", "#-ab", "#.ab", "#a b", "##ab", "###ab", "#ab##", "#é", "#éx", "#漢字",
		"#áb", "#́ab", "hello", "-hello", "hello*", "*hello", "**hello**", "...word",
		"word...", "\"quoted\"", "-\"quoted\"", "a:b", ":", "-", "--", "-#tag", "#tag-",
		"(word)", "[word]", "{word}", "!!!word???", "word!", "word*", "word**", "word\"",
		"\"word", "word ", " word", "٣word", "word٣", "word́",
		"́word", "word​", "​word", "1234", "-1234", "1234-", "café", "café!",
		"#café", "@user", "@user!", "http://example.com/", "a.b.c", "...", "***", "\"\"",
	}
	var wordCases []map[string]any
	for _, w := range words {
		wordCases = append(wordCases, map[string]any{
			"in":                     w,
			"search_term_punc_start": start.ReplaceAllString(w, ""),
			"search_term_punc_end":   end.ReplaceAllString(w, ""),
			"hashtag_start":          hashStart.ReplaceAllString(w, "#"),
			"valid_hashtag":          hashValid.MatchString(w),
			// The exact composition parseSearchFlags applies, in its order.
			"trimmed": hashStart.ReplaceAllString(
				end.ReplaceAllString(start.ReplaceAllString(w, ""), ""), "#"),
		})
	}

	return map[string]any{
		"search_term_punc_start": startCases,
		"search_term_punc_end":   endCases,
		"words":                  wordCases,
	}
}

// --- strings.Fields -----------------------------------------------------------------------------

// spStringsFieldsAll pins the splitter splitWords falls back to. It is `unicode.IsSpace`, which
// is a *different* set from the `\s` in the regexes above — U+00A0 splits a field but is not
// stripped as punctuation.
func spStringsFieldsAll() map[string]any {
	var sweep []map[string]any
	for _, r := range spProbeRunes() {
		in := "a" + string(r) + "b"
		sweep = append(sweep, map[string]any{
			"rune":   int(r),
			"in":     in,
			"fields": strings.Fields(in),
		})
	}

	corpora := []string{
		"", " ", "   ", "a b", " a b ", "a\tb", "a\nb", "a\vb", "a\fb", "a\rb",
		"ab", "a b", "a b", "a​b", "a　b", "a  b   c",
	}
	var cases []map[string]any
	for _, c := range corpora {
		cases = append(cases, map[string]any{"in": c, "fields": strings.Fields(c)})
	}

	return map[string]any{"sweep": sweep, "corpus": cases}
}

// --- PadDateStringZeros -------------------------------------------------------------------------

func spPadAll() []map[string]any {
	inputs := []string{
		"", "-", "--", "1", "12", "123", "1234", "2019-1-2", "2019-01-02", "2019-1-02",
		"2019-01-2", "19-1-2", "2019-12-31", "2019-013-02", "2019-1-2-3", "-2019-1-2",
		"2019-1-", "2019--2", "a-b-c", "2019-1-2 ", " 2019-1-2", "2019/1/2", "0-0-0",
		"2019-0-0", "9-9-9", "٣-١-٢",
	}
	var res []map[string]any
	for _, in := range inputs {
		res = append(res, map[string]any{"in": in, "out": model.PadDateStringZeros(in)})
	}
	return res
}

// --- GetStartOfDayMillis / GetEndOfDayMillis ----------------------------------------------------

// spDayMillisAll drives the two zone helpers directly. The offsets deliberately run past a whole
// day in both directions: `time.FixedZone` takes seconds and does not range-check, while chrono's
// FixedOffset stops at ±86399, so the port has to do the arithmetic rather than delegate.
func spDayMillisAll() []map[string]any {
	instants := []struct {
		name string
		t    time.Time
	}{
		{"epoch", time.Unix(0, 0).UTC()},
		{"iso_2019_01_02", time.Date(2019, 1, 2, 0, 0, 0, 0, time.UTC)},
		// A non-zero clock, to prove only Y/M/D survive.
		{"mid_day", time.Date(2019, 1, 2, 13, 45, 56, 123456789, time.UTC)},
		{"end_of_year", time.Date(2019, 12, 31, 23, 59, 59, 999999999, time.UTC)},
		{"leap_day", time.Date(2024, 2, 29, 6, 0, 0, 0, time.UTC)},
		{"pre_epoch", time.Date(1969, 7, 20, 20, 17, 0, 0, time.UTC)},
		{"year_1", time.Date(1, 1, 1, 0, 0, 0, 0, time.UTC)},
		{"far_future", time.Date(9999, 12, 31, 0, 0, 0, 0, time.UTC)},
		// Read in a non-UTC zone: Year/Month/Day come off the *wall clock* of that zone.
		{"wall_clock_in_plus_14", time.Date(2019, 1, 2, 1, 0, 0, 0, time.FixedZone("+14", 14*3600))},
	}
	offsets := []int{0, 1, -1, 3600, -3600, 19800, -19800, 43200, -43200, 50400, -43200,
		86399, -86399, 86400, -86400, 100000, -100000, 1000000, -1000000}

	var res []map[string]any
	for _, in := range instants {
		for _, off := range offsets {
			res = append(res, map[string]any{
				"name":   in.name,
				"offset": off,
				// The Y/M/D the helpers actually read, so a Rust failure says which half is wrong.
				"year":  in.t.Year(),
				"month": int(in.t.Month()),
				"day":   in.t.Day(),
				"start": model.GetStartOfDayMillis(in.t, off),
				"end":   model.GetEndOfDayMillis(in.t, off),
			})
		}
	}
	return res
}

// --- the six date accessors ---------------------------------------------------------------------

// spDateMillisAll runs one date string through all six accessors. `GetAfterDateMillis` and
// `GetExcludedAfterDateMillis` fall back to `time.Now()` when the parse fails, so those rows carry
// `uses_now` and no value: a clock-derived number in a committed fixture would either be wrong
// tomorrow or force the fixture to be regenerated daily.
func spDateMillisAll() []map[string]any {
	dates := []string{
		"", "2019-01-02", "2019-1-2", "2019-12-31", "2024-02-29", "2023-02-29", "2019-13-01",
		"2019-00-01", "2019-01-00", "2019-01-32", "19-01-02", "2019-01-02T00:00:00Z",
		"2019-01-02 ", " 2019-01-02", "not-a-date", "0001-01-01", "9999-12-31", "1970-01-01",
		"1969-12-31", "2019-01-02-03", "٢٠١٩-٠١-٠٢",
	}
	offsets := []int{0, 3600, -3600, 19800, 86400, -100000}

	var res []map[string]any
	for _, d := range dates {
		for _, off := range offsets {
			// Whether Go's own parse succeeds decides which accessors are clock-dependent.
			_, err := time.Parse("2006-01-02", model.PadDateStringZeros(d))
			parses := err == nil

			row := map[string]any{
				"date":   d,
				"offset": off,
				"parses": parses,
			}

			after := &model.SearchParams{AfterDate: d, TimeZoneOffset: off}
			exAfter := &model.SearchParams{ExcludedAfterDate: d, TimeZoneOffset: off}
			before := &model.SearchParams{BeforeDate: d, TimeZoneOffset: off}
			exBefore := &model.SearchParams{ExcludedBeforeDate: d, TimeZoneOffset: off}
			on := &model.SearchParams{OnDate: d, TimeZoneOffset: off}
			exOn := &model.SearchParams{ExcludedDate: d, TimeZoneOffset: off}

			if parses {
				row["after"] = after.GetAfterDateMillis()
				row["excluded_after"] = exAfter.GetExcludedAfterDateMillis()
			} else {
				// Recorded as a flag; the Rust side recomputes against its own clock.
				row["uses_now"] = true
			}
			row["before"] = before.GetBeforeDateMillis()
			row["excluded_before"] = exBefore.GetExcludedBeforeDateMillis()
			onStart, onEnd := on.GetOnDateMillis()
			row["on_start"], row["on_end"] = onStart, onEnd
			exStart, exEnd := exOn.GetExcludedDateMillis()
			row["excluded_start"], row["excluded_end"] = exStart, exEnd

			res = append(res, row)
		}
	}
	return res
}

// --- ParseSearchParams ---------------------------------------------------------------------------

// spParseCorpus is the whole point of the file. Each entry targets a specific branch of the two
// unexported helpers, observable only through the result:
//
//   - quoting: closed, unclosed, empty, adjacent, `-` immediately before an opening quote
//   - flags: known and unknown names, case folding, empty value consuming the next word,
//     an empty value at the end of input (which falls through and becomes a *term*),
//     a `-` prefix, a leading colon, several colons
//   - terms: hashtags against plain words, exclusions, wildcards, punctuation trimming
//   - the three-way output: plain params, hashtag params, and the filter-only special case
var spParseCorpus = []string{
	"",
	"   ",
	"hello",
	"hello world",
	"  hello   world  ",
	"-hello",
	"hello -world",
	"#tag",
	"#tag word",
	"-#tag",
	"#tag -#other",
	"#tag -word",
	"#a",
	"##tag",
	"###tag",
	"#tag##",
	"hello*",
	"*hello",
	"hel*lo",
	"...hello...",
	"!!!hello???",
	`"quoted phrase"`,
	`"quoted phrase" plain`,
	`plain "quoted phrase"`,
	`-"quoted phrase"`,
	`a-"quoted"`,
	`"unclosed`,
	`"unclosed phrase`,
	`""`,
	`"" ""`,
	`a"b"c`,
	`"a""b"`,
	`" "`,
	"in:town-square",
	"in:town-square hello",
	"-in:town-square",
	"channel:town-square",
	"IN:town-square",
	"In:Town-Square",
	"from:alice",
	"-from:alice",
	"from:alice in:town-square hello",
	"ext:txt",
	"-ext:txt",
	"before:2019-01-02",
	"after:2019-01-02",
	"on:2019-01-02",
	"-before:2019-01-02",
	"-after:2019-01-02",
	"-on:2019-01-02",
	// An empty value consumes the next word...
	"in: town-square",
	"from: alice hello",
	// ...unless there is no next word, in which case it becomes a term.
	"in:",
	"from:",
	"hello in:",
	"-in:",
	// Not a known flag: falls through to the term path, and the trailing colon is trimmed.
	"unknown:value",
	"unknown: value",
	":value",
	"-:value",
	"::",
	"a:b:c",
	"in:a:b",
	"in:town-square in:other",
	"from:alice from:bob",
	"ext:txt ext:pdf",
	"on:2019-01-02 on:2020-01-02",
	"in:town-square -in:other",
	// Filter with no terms at all — the third params block.
	"in:town-square",
	"after:2019-01-02 before:2019-02-03",
	"ext:txt",
	// Both a plain term and a hashtag: two params blocks, sharing the same filters.
	"#tag word in:town-square",
	`#tag "quoted" -#other -word in:a -from:b ext:c after:2019-01-02`,
	// Unicode: the splitter and the trimmer disagree about U+00A0.
	"café",
	"café!",
	"#café",
	"a b",
	"a　b",
	"٣hello",
	"hello٣",
	"​hello",
	"漢字",
	"#漢字",
	"😀",
	"á",
	// Quotes that are not ASCII quotes are just punctuation.
	"“smart quoted”",
	// A minus that is not ASCII hyphen does not exclude.
	"−hello",
	"-",
	"--",
	"- hello",
	"-#",
	"#",
	"##",
}

func spParseAll() []map[string]any {
	offsets := []int{0, 19800}
	var res []map[string]any
	for _, text := range spParseCorpus {
		for _, off := range offsets {
			row := map[string]any{"in": text, "offset": off}
			probe(row, func() {
				params := model.ParseSearchParams(text, off)
				row["count"] = len(params)
				row["out"] = mustMarshal(params)
				// The wire form hides every empty slice behind omitempty, so record the
				// nil-ness the Rust port has to reproduce for at least one block.
				if len(params) > 0 {
					row["in_channels_nil"] = params[0].InChannels == nil
					row["extensions_nil"] = params[0].Extensions == nil
				}
			})
			res = append(res, row)
		}
	}
	return res
}

// --- IsSearchParamsListValid ---------------------------------------------------------------------

func spIsValidListAll() []map[string]any {
	yes := &model.SearchParams{IncludeDeletedChannels: true}
	no := &model.SearchParams{IncludeDeletedChannels: false}

	lists := []struct {
		name string
		in   []*model.SearchParams
	}{
		{"nil", nil},
		{"empty", []*model.SearchParams{}},
		{"one_true", []*model.SearchParams{yes}},
		{"one_false", []*model.SearchParams{no}},
		{"all_true", []*model.SearchParams{yes, yes, yes}},
		{"all_false", []*model.SearchParams{no, no}},
		{"mixed_true_first", []*model.SearchParams{yes, no}},
		{"mixed_false_first", []*model.SearchParams{no, yes}},
		{"mixed_late", []*model.SearchParams{no, no, no, yes}},
	}

	var res []map[string]any
	for _, l := range lists {
		row := map[string]any{"name": l.name}
		probe(row, func() {
			err := model.IsSearchParamsListValid(l.in)
			if err == nil {
				row["err"] = nil
				return
			}
			row["err"] = map[string]any{
				"id":          err.Id,
				"where":       err.Where,
				"status_code": err.StatusCode,
				"message":     err.Message,
			}
		})
		res = append(res, row)
	}
	return res
}

// --- the wire format ------------------------------------------------------------------------------

// spWireAll pins omitempty. Every field carries it except Modifier, so a zero SearchParams is
// `{"modifier":""}` rather than an object of twenty nulls — and an empty slice disappears exactly
// as a nil one does, which is the opposite of PostList's collections.
func spWireAll() []map[string]any {
	cases := []struct {
		name string
		p    *model.SearchParams
	}{
		{"zero", &model.SearchParams{}},
		{"empty_slices", &model.SearchParams{
			InChannels: []string{}, ExcludedChannels: []string{},
			FromUsers: []string{}, ExcludedUsers: []string{},
			Extensions: []string{}, ExcludedExtensions: []string{},
		}},
		{"terms_only", &model.SearchParams{Terms: "hello"}},
		{"false_bools", &model.SearchParams{
			Terms: "x", IsHashtag: false, OrTerms: false,
			IncludeDeletedChannels: false, SearchWithoutUserId: false,
		}},
		{"true_bools", &model.SearchParams{
			IsHashtag: true, OrTerms: true,
			IncludeDeletedChannels: true, SearchWithoutUserId: true,
		}},
		{"zero_offset", &model.SearchParams{TimeZoneOffset: 0}},
		{"negative_offset", &model.SearchParams{TimeZoneOffset: -19800}},
		{"modifier", &model.SearchParams{Modifier: "messages"}},
		{"escapes", &model.SearchParams{Terms: `a<b>c&d "e"`, Modifier: "<x>"}},
		{"everything", &model.SearchParams{
			Terms: "t", ExcludedTerms: "et", IsHashtag: true,
			InChannels: []string{"c1", "c2"}, ExcludedChannels: []string{"c3"},
			FromUsers: []string{"u1"}, ExcludedUsers: []string{"u2"},
			AfterDate: "2019-01-02", ExcludedAfterDate: "2019-01-03",
			BeforeDate: "2019-02-02", ExcludedBeforeDate: "2019-02-03",
			Extensions: []string{"txt"}, ExcludedExtensions: []string{"pdf"},
			OnDate: "2019-03-02", ExcludedDate: "2019-03-03",
			OrTerms: true, IncludeDeletedChannels: true, TimeZoneOffset: 19800,
			SearchWithoutUserId: true, Modifier: "files",
		}},
	}

	docs := []string{
		`{}`,
		`{"modifier":"messages"}`,
		`{"terms":"hello","in_channels":["a"],"timezone_offset":19800}`,
		`{"in_channels":null,"extensions":null}`,
		`{"in_channels":[],"extensions":[]}`,
		`{"ishashtag":true,"or_terms":true,"include_deleted_channels":true,"search_without_user_id":true}`,
		`{"timezone_offset":-19800}`,
		`{"unknown_key":1,"terms":"x"}`,
		// Go's encoding/json leaves the value untouched when it sees null, for every type —
		// not only slices. serde rejects all of these; see [D-057].
		`{"terms":null,"modifier":null}`,
		`{"ishashtag":null,"timezone_offset":null}`,
	}

	var res []map[string]any
	for _, c := range cases {
		res = append(res, map[string]any{
			"name": c.name,
			"kind": "marshal",
			"out":  mustMarshal(c.p),
		})
	}
	for _, doc := range docs {
		row := map[string]any{"name": doc, "kind": "round_trip", "in": doc}
		probe(row, func() {
			var p model.SearchParams
			if err := json.Unmarshal([]byte(doc), &p); err != nil {
				row["err"] = err.Error()
				return
			}
			row["out"] = mustMarshal(&p)
			row["in_channels_nil"] = p.InChannels == nil
			row["extensions_nil"] = p.Extensions == nil
		})
		res = append(res, row)
	}
	return res
}
