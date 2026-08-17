package main

// Generator for crates/mm-model/src/locale_generated.rs, and the behavioural oracle for
// model.IsValidLocale. Closes [D-001].
//
// # Why a generated table rather than a rule written by hand
//
// `IsValidLocale` is `len <= 5` plus `language.Parse`, and `language.Parse` validates against the
// **IANA subtag registry** rather than against BCP 47 syntax. So `xx` is syntactically perfect
// and rejected, because it is not a registered language. There is no rule to write; the registry
// is the rule.
//
// # Why the whole input space is enumerated
//
// `UserLocaleMaxLength` is 5, so the reachable input space is every string of at most five bytes.
// Restricted to the characters that can appear in a tag — `[a-z0-9-_]` — that is 81,376,658
// strings, and `language.Parse` answers all of them in about eight seconds. So the accepted set is
// not sampled or reasoned about: it is enumerated.
//
// # Why the emitted tables are not the accepted set
//
// 234,421 of those strings are accepted, which is too many to emit as a list. They decompose:
//
//	root                          1
//	2-letter languages          190
//	3-letter languages        8,866
//	ll<sep>RR                62,130 x 2 separators   (190 languages x 327 regions)
//	x-... private use        the rest, rule-shaped
//
// So the generator emits the three *component* tables — about 9,400 entries — plus a rule, and
// then **verifies the rule reproduces `language.Parse` on all 81 million inputs**. The
// enumeration becomes the proof rather than the payload. If the rule and Go ever disagree on one
// input, generation fails rather than emitting a table that is quietly wrong.

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"golang.org/x/text/language"
)

// The characters that can appear in a tag of five bytes or fewer. Anything outside this set is
// probed separately in the behaviour fixture and rejected.
const localeAlphabet = "abcdefghijklmnopqrstuvwxyz0123456789-_"

type localeTables struct {
	languages2 []string
	languages3 []string
	regions2   []string
}

func isASCIILetter(c byte) bool { return c >= 'a' && c <= 'z' }
func isASCIIDigit(c byte) bool  { return c >= '0' && c <= '9' }
func isAlnum(c byte) bool       { return isASCIILetter(c) || isASCIIDigit(c) }
func isSep(c byte) bool         { return c == '-' || c == '_' }

// localeRuleAccepts is the candidate rule, and is exactly what the Rust port implements. It is
// checked against language.Parse for every input below.
func localeRuleAccepts(s string, t *localeTables, in map[string]bool) bool {
	n := len(s)
	if n == 0 || n > 5 {
		return n == 0 // IsValidLocale short-circuits "" to true before Parse is reached.
	}

	// Anything the structural rules below cannot express is listed verbatim. In practice this is
	// the registry's **grandfathered** tags — `i-ami`, `i-hak`, `i-lux` and five others, each in
	// both separator spellings. They are irregular by definition, which is why they are a list
	// rather than a rule, and why the list is derived from the enumeration rather than typed out:
	// the generator emits whatever its own rule fails to cover.
	if in["ex:"+s] {
		return true
	}

	if s == "root" {
		return true
	}

	// Private use: `x` followed by one or more separator-delimited alphanumeric subtags.
	if n >= 3 && (s[0] == 'x' || s[0] == 'X') && isSep(s[1]) {
		parts := strings.FieldsFunc(s[2:], func(r rune) bool { return r == '-' || r == '_' })
		if len(parts) == 0 {
			return false
		}
		// FieldsFunc drops empties, so re-check the raw form has no empty subtag.
		for _, part := range parts {
			if part == "" || len(part) > 8 {
				return false
			}
			for i := range len(part) {
				if !isAlnum(part[i]) {
					return false
				}
			}
		}
		// Reject a trailing or doubled separator, which FieldsFunc would have hidden.
		rest := s[2:]
		if rest == "" || isSep(rest[0]) || isSep(rest[len(rest)-1]) {
			return false
		}
		for i := 1; i < len(rest); i++ {
			if isSep(rest[i]) && isSep(rest[i-1]) {
				return false
			}
		}
		return true
	}

	// A bare language subtag.
	if n == 2 && isASCIILetter(s[0]) && isASCIILetter(s[1]) {
		return in["l2:"+s]
	}
	if n == 3 && isASCIILetter(s[0]) && isASCIILetter(s[1]) && isASCIILetter(s[2]) {
		return in["l3:"+s]
	}

	// language + separator + region.
	if n == 5 && isASCIILetter(s[0]) && isASCIILetter(s[1]) && isSep(s[2]) &&
		isASCIILetter(s[3]) && isASCIILetter(s[4]) {
		return in["l2:"+s[:2]] && in["r2:"+s[3:]]
	}

	return false
}

func writeLocaleGenerated(outDir, rustOutDir string) error {
	// --- 1. enumerate --------------------------------------------------------------------------
	accepted := make([]string, 0, 250_000)
	buf := make([]byte, 0, 5)
	var walk func(depth int)
	walk = func(depth int) {
		if len(buf) > 0 {
			if _, err := language.Parse(string(buf)); err == nil {
				accepted = append(accepted, string(buf))
			}
		}
		if depth == 5 {
			return
		}
		for i := range len(localeAlphabet) {
			buf = append(buf, localeAlphabet[i])
			walk(depth + 1)
			buf = buf[:len(buf)-1]
		}
	}
	walk(0)

	// --- 2. decompose --------------------------------------------------------------------------
	l2 := map[string]bool{}
	l3 := map[string]bool{}
	r2 := map[string]bool{}
	for _, v := range accepted {
		switch {
		case len(v) == 2:
			l2[v] = true
		case len(v) == 3 && isASCIILetter(v[0]) && isASCIILetter(v[1]) && isASCIILetter(v[2]):
			l3[v] = true
		case len(v) == 5 && isSep(v[2]) && isASCIILetter(v[3]) && isASCIILetter(v[4]) &&
			isASCIILetter(v[0]) && isASCIILetter(v[1]):
			r2[v[3:]] = true
		}
	}

	tables := &localeTables{}
	for k := range l2 {
		tables.languages2 = append(tables.languages2, k)
	}
	for k := range l3 {
		tables.languages3 = append(tables.languages3, k)
	}
	for k := range r2 {
		tables.regions2 = append(tables.regions2, k)
	}
	sort.Strings(tables.languages2)
	sort.Strings(tables.languages3)
	sort.Strings(tables.regions2)

	index := map[string]bool{}
	for _, v := range tables.languages2 {
		index["l2:"+v] = true
	}
	for _, v := range tables.languages3 {
		index["l3:"+v] = true
	}
	for _, v := range tables.regions2 {
		index["r2:"+v] = true
	}

	// --- 3. residual: whatever the structural rules cannot express --------------------------------
	//
	// Rather than guess which irregular tags the registry carries, ask: which accepted inputs does
	// the rule reject? Those become the exception table. The verification below then has to pass
	// with them included, so a mistake here cannot hide.
	exceptions := []string{}
	for _, v := range accepted {
		if !localeRuleAccepts(v, tables, index) {
			exceptions = append(exceptions, v)
		}
	}
	sort.Strings(exceptions)
	for _, v := range exceptions {
		index["ex:"+v] = true
	}

	// --- 4. verify the rule against every input ------------------------------------------------
	//
	// This is the step that makes the tables trustworthy. A disagreement on any one of the 81
	// million inputs fails the build rather than emitting a table that is quietly wrong.
	mismatches := 0
	var examples []string
	buf = buf[:0]
	var verify func(depth int)
	verify = func(depth int) {
		if len(buf) > 0 {
			s := string(buf)
			_, err := language.Parse(s)
			want := err == nil
			got := localeRuleAccepts(s, tables, index)
			if want != got {
				mismatches++
				if len(examples) < 10 {
					examples = append(examples, fmt.Sprintf("%q go=%v rule=%v", s, want, got))
				}
			}
		}
		if depth == 5 {
			return
		}
		for i := range len(localeAlphabet) {
			buf = append(buf, localeAlphabet[i])
			verify(depth + 1)
			buf = buf[:len(buf)-1]
		}
	}
	verify(0)

	if mismatches > 0 {
		return fmt.Errorf(
			"locale rule disagrees with language.Parse on %d of %d inputs; examples: %s",
			mismatches, 81376658, strings.Join(examples, "; "))
	}

	localeExceptions = exceptions

	// --- 5. emit the Rust table ----------------------------------------------------------------
	var b strings.Builder
	b.WriteString(`//! Generated by reference/dump/locale_gen.go — DO NOT EDIT.
//!
//! The IANA subtag registry, as far as ` + "`model.IsValidLocale`" + ` can reach it.
//!
//! ` + "`UserLocaleMaxLength`" + ` is 5, so the reachable input space is every string of at most five
//! bytes. Over the characters a tag can contain, that is 81,376,658 strings; the generator asks
//! ` + "`golang.org/x/text/language.Parse`" + ` about every one of them, decomposes the 234,421 it accepts
//! into these three tables, and then **re-derives all 81 million answers from the tables** and
//! fails if a single one disagrees. So these are not a sample.
//!
//! Never hand-edit; re-run the generator. Each table carries ` + "`#[rustfmt::skip]`" + ` so ` + "`cargo fmt`" + `
//! and the generator stay idempotent against each other, the same as ` + "`emoji_generated.rs`" + `.

`)
	emit := func(name, doc string, values []string) {
		fmt.Fprintf(&b, "/// %s\n#[rustfmt::skip]\npub static %s: &[&str] = &[\n", doc, name)
		for i := 0; i < len(values); i += 8 {
			end := min(i+8, len(values))
			b.WriteString("    ")
			for _, v := range values[i:end] {
				fmt.Fprintf(&b, "%q, ", v)
			}
			b.WriteString("\n")
		}
		b.WriteString("];\n\n")
	}
	emit("LANGUAGES_2", fmt.Sprintf("Two-letter language subtags the registry knows (%d).", len(tables.languages2)), tables.languages2)
	emit("LANGUAGES_3", fmt.Sprintf("Three-letter language subtags the registry knows (%d).", len(tables.languages3)), tables.languages3)
	emit("REGIONS_2", fmt.Sprintf("Two-letter region subtags the registry knows (%d).", len(tables.regions2)), tables.regions2)
	emit("EXCEPTIONS", fmt.Sprintf(
		"Accepted tags no structural rule covers (%d) — the registry's grandfathered entries, in both separator spellings.",
		len(exceptions)), exceptions)

	rustPath := filepath.Join(rustOutDir, "locale_generated.rs")
	if err := os.WriteFile(rustPath, []byte(b.String()), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s (%d + %d + %d + %d entries, rule verified against %d inputs)\n",
		rustPath, len(tables.languages2), len(tables.languages3), len(tables.regions2),
		len(exceptions), 81376658)

	// --- 5. the behaviour fixture --------------------------------------------------------------
	return writeLocaleBehaviourFixture(outDir, accepted)
}

// localeExceptions is filled by writeLocaleGenerated so the fixture can record it.
var localeExceptions []string

func writeLocaleBehaviourFixture(outDir string, accepted []string) error {
	// The hand-picked cases from D-001, plus the shapes the rule turns on.
	probes := []string{
		"", "en", "eng", "zh-CN", "pt-br", "en_US", "root", "und", "qaa", "mul", "zxx",
		"xx", "xxx", "engl", "zh-Ha", "en-1", "i-en", "a-b", "C", "POSIX",
		// Length boundary: six bytes is rejected before Parse is reached.
		"en-USA", "en_USA",
		// Case variants of an accepted tag.
		"EN", "En", "eN", "EN_US", "en_us", "EN-us",
		// Separator variants.
		"en-US", "en_US",
		// Private use.
		"x-a", "x-ab", "x-abc", "x-a-b", "x_a_b", "x-", "x--a",
		// Characters outside the tag alphabet.
		"en US", "en.US", "en+US", "én", "en\tUS", "en/US",
		// Digits.
		"1", "12", "123", "en-99", "99",
	}

	type probeResult struct {
		Input string `json:"input"`
		Valid bool   `json:"valid"`
		Parse bool   `json:"parse_ok"`
	}
	results := make([]probeResult, 0, len(probes))
	for _, p := range probes {
		_, err := language.Parse(p)
		results = append(results, probeResult{
			Input: p,
			Valid: modelIsValidLocale(p),
			Parse: err == nil,
		})
	}

	// A sample of the accepted set, spread across the range, so the Rust side checks real
	// membership rather than only the hand-picked cases. Deterministic: every 2,000th entry.
	sample := make([]string, 0, 128)
	for i := 0; i < len(accepted); i += 2000 {
		sample = append(sample, accepted[i])
	}

	out := map[string]any{
		"max_length":      5,
		"exceptions":      localeExceptions,
		"probes":          results,
		"accepted_total":  len(accepted),
		"accepted_sample": sample,
	}
	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	path := filepath.Join(outDir, "behaviour_locale.json")
	if err := os.WriteFile(path, append(blob, '\n'), 0o644); err != nil {
		return err
	}
	fmt.Printf("wrote %s\n", path)
	return nil
}

// modelIsValidLocale mirrors model.IsValidLocale, which is what the fixture records.
func modelIsValidLocale(locale string) bool {
	if locale != "" {
		if len(locale) > 5 {
			return false
		} else if _, err := language.Parse(locale); err != nil {
			return false
		}
	}
	return true
}
