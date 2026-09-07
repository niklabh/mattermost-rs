package main

// Behavioural oracle for `encoding/json`'s case-insensitive field matching — [D-040] — written to
// fixtures/behaviour_json_fold.json.
//
// # The rule, from decode.go:699
//
//	f := fields.byExactName[string(key)]
//	if f == nil {
//	    f = fields.byFoldedName[string(foldName(key))]
//	}
//
// So: an **exact** match on the JSON name wins; failing that, a match on `foldName`. And the
// folded index is built with "first folded match takes precedence" (encode.go:1306), meaning
// **declaration order** decides a fold collision — the same rule, applied to two different maps.
//
// # What is actually recorded, and why it is not just a table of foldName
//
// Three sections:
//
//  1. `fold_name` — `foldName` over a corpus. ASCII lower-cases fold up; everything else goes
//     through `foldRune`, which returns the *smallest* rune of the SimpleFold orbit.
//
//  2. `ascii_fold_sources` — **the complete set of non-ASCII runes whose fold lands on an ASCII
//     byte**, swept over the whole Unicode range. This is the section that makes a partial port
//     defensible: every `json:` name in the Mattermost tree is ASCII, so a non-ASCII key can only
//     ever match one if its fold is ASCII, and that set turns out to be tiny. A port that carries
//     this table is exact for every comparison against an ASCII field name — which is all of
//     them — without reproducing `unicode.SimpleFold` in full.
//
//  3. `decode` — end-to-end `json.Unmarshal` outcomes against a struct shaped like
//     `MessageAttachment`: which field each key populated, or none. This is the section the Rust
//     port is actually asserted against, because it covers the *precedence* rules that no amount
//     of folding on its own would show: exact beats folded, and declaration order breaks a folded
//     tie.
//
// Determinism: fixed corpora and a whole-range sweep. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"unicode"
	"unicode/utf8"
)

// --- foldName, transcribed from encoding/json/fold.go ------------------------------------------
//
// Unexported there, so it is copied here character for character — the same arrangement
// behaviour.go uses for the model package's identifier regexes, and the same standing rule:
// copy any upstream change verbatim, because the Rust side asserts against these results.

func foldName(in []byte) []byte {
	return appendFoldedName(make([]byte, 0, 32), in)
}

func appendFoldedName(out, in []byte) []byte {
	for i := 0; i < len(in); {
		if c := in[i]; c < utf8.RuneSelf {
			if 'a' <= c && c <= 'z' {
				c -= 'a' - 'A'
			}
			out = append(out, c)
			i++
			continue
		}
		r, n := utf8.DecodeRune(in[i:])
		out = utf8.AppendRune(out, foldRune(r))
		i += n
	}
	return out
}

func foldRune(r rune) rune {
	for {
		r2 := unicode.SimpleFold(r)
		if r2 <= r {
			return r2
		}
		r = r2
	}
}

// --- the corpora --------------------------------------------------------------------------------

// foldCorpus covers the shapes a JSON key can take against an ASCII field name: the exact tag,
// every casing of it, the two non-ASCII runes that fold into ASCII, a rune that folds to a
// non-ASCII rune, and the separators Mattermost tags actually use.
var foldCorpus = []string{
	"", "title", "Title", "TITLE", "tItLe", "TiTlE",
	"author_name", "Author_Name", "AUTHOR_NAME", "authorname", "author-name",
	"ts", "TS", "Ts", "tS",
	"id", "ID", "iD",
	"k", "K", "K", // KELVIN SIGN
	"s", "S", "ſ", // LATIN SMALL LETTER LONG S
	"µ", "μ", "Σ", "σ", "ς", // micro, mu, sigma forms
	"İ", "ı", // dotted/dotless I
	"café", "CAFÉ",
	"a1", "A1", "_", "__", "-",
}

// asciiFoldSources sweeps the whole scalar range for runes whose fold is an ASCII byte. The
// answer is the only thing a Rust port needs in order to be exact against ASCII field names.
func asciiFoldSources() []map[string]any {
	rows := make([]map[string]any, 0, 8)
	for r := rune(0); r <= unicode.MaxRune; r++ {
		if r >= utf8.RuneSelf && !utf8.ValidRune(r) {
			continue
		}
		if r < utf8.RuneSelf {
			continue // ASCII is handled by the byte branch, not by foldRune
		}
		folded := foldRune(r)
		if folded < utf8.RuneSelf {
			rows = append(rows, map[string]any{
				"rune":   int(r),
				"folded": int(folded),
				"name":   fmt.Sprintf("U+%04X", r),
			})
		}
	}
	return rows
}

// attachmentLike mirrors model.MessageAttachment's tags closely enough to exercise the precedence
// rules, and adds the one shape the real struct cannot: two names that collide under folding.
// `Title` is declared before `TITLE`, so the folded index keeps the first.
type attachmentLike struct {
	Id         int64  `json:"id"`
	Title      string `json:"title"`
	TitleUpper string `json:"TITLE"`
	TitleLink  string `json:"title_link"`
	AuthorName string `json:"author_name"`
	Text       string `json:"text"`
	Ts         any    `json:"ts"`
}

// decodeCorpus is a list of whole JSON documents, so the recorded answer includes which field a
// key landed in — the thing the port has to reproduce, rather than the fold in isolation.
var decodeCorpus = []string{
	`{"title":"t"}`,
	`{"Title":"t"}`,
	`{"TITLE":"t"}`,
	`{"tItLe":"t"}`,
	`{"title":"exact","TITLE":"upper"}`,
	`{"TITLE":"upper","title":"exact"}`,
	`{"TiTlE":"folded","title":"exact"}`,
	`{"title":"exact","TiTlE":"folded"}`,
	`{"TiTlE":"a","tItLe":"b"}`,
	`{"AUTHOR_NAME":"a"}`,
	`{"Author_Name":"a"}`,
	`{"authorname":"a"}`,
	`{"author-name":"a"}`,
	`{"TITLE_LINK":"l"}`,
	`{"Title_Link":"l"}`,
	`{"TEXT":"x","TITLE":"t"}`,
	`{"ID":7}`,
	`{"Id":7}`,
	`{"TS":123}`,
	`{"Key":"k"}`,
	`{"ſ":"s"}`,
	// The two non-ASCII runes that fold into ASCII, each aimed at a field it can actually reach:
	// U+017F LATIN SMALL LETTER LONG S folds to "S", so "tſ" folds to "TS" and hits `ts`; U+212A
	// KELVIN SIGN folds to "K", so "title_linK" hits `title_link`. Both are the whole reason the
	// ascii_fold_sources sweep above exists, and without a positive case the table it produces
	// would be a claim rather than a demonstration.
	`{"t\u017f":123}`,
	`{"title_lin\u212a":"l"}`,
	`{"tit\u017fe":"no-such-field"}`,
	`{"nosuchkey":"x"}`,
	`{"":"x"}`,
}

func fieldsOf(v attachmentLike) map[string]any {
	return map[string]any{
		"id":          v.Id,
		"title":       v.Title,
		"TITLE":       v.TitleUpper,
		"title_link":  v.TitleLink,
		"author_name": v.AuthorName,
		"text":        v.Text,
		"ts":          v.Ts,
	}
}

// sortKeys re-emits a JSON object with its keys in bytewise order, values untouched.
//
// `map[string]json.RawMessage` loses the author order, which is the point: what comes back out is
// whatever order this function writes. A non-object, or anything that will not decode, is returned
// unchanged so the corpus can carry malformed inputs.
func sortKeys(document string) string {
	var raw map[string]json.RawMessage
	if err := json.Unmarshal([]byte(document), &raw); err != nil {
		return document
	}
	keys := make([]string, 0, len(raw))
	for k := range raw {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	out := []byte{'{'}
	for i, k := range keys {
		if i > 0 {
			out = append(out, ',')
		}
		name, err := json.Marshal(k)
		if err != nil {
			return document
		}
		out = append(out, name...)
		out = append(out, ':')
		out = append(out, raw[k]...)
	}
	return string(append(out, '}'))
}

func writeJSONFoldBehaviourFixture(outDir string) error {
	folds := make([]map[string]any, 0, len(foldCorpus))
	for _, in := range foldCorpus {
		folds = append(folds, map[string]any{
			"in":  in,
			"out": string(foldName([]byte(in))),
		})
	}

	decodes := make([]map[string]any, 0, len(decodeCorpus))
	for _, doc := range decodeCorpus {
		row := map[string]any{"in": doc}

		var got attachmentLike
		err := json.Unmarshal([]byte(doc), &got)
		row["failed"] = err != nil
		row["out"] = fieldsOf(got)

		// The same document with its keys sorted bytewise — the order a `serde_json::Map`
		// (a BTreeMap) presents them in, and the order Postgres `jsonb` stores equal-length keys
		// in. Go resolves keys in the order it reads them and the last assignment to a field
		// wins, so this is the answer that matters to a server reading these props out of the
		// shared column rather than off the wire.
		sorted := sortKeys(doc)
		var gotSorted attachmentLike
		errSorted := json.Unmarshal([]byte(sorted), &gotSorted)
		row["sorted_in"] = sorted
		row["sorted_failed"] = errSorted != nil
		row["sorted_out"] = fieldsOf(gotSorted)

		decodes = append(decodes, row)
	}

	out := map[string]any{
		"fold_name":          folds,
		"ascii_fold_sources": asciiFoldSources(),
		"decode":             decodes,
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_json_fold.json"), append(blob, '\n'), 0o644)
}
