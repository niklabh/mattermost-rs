package main

// Behavioural oracle for model/unicode.go, written to fixtures/behaviour_unicode.json — and the
// generator for crates/mm-model/src/unicode_generated.rs, which is Rust source rather than a
// fixture. Same arrangement as behaviour_emoji.go: the table is emitted, not transcribed.
//
// `ContainsCJK` is four lines and there is nothing to reason about in them:
//
//	unicode.Is(unicode.Han, r) || unicode.Is(unicode.Hiragana, r) ||
//	unicode.Is(unicode.Katakana, r) || unicode.Is(unicode.Hangul, r)
//
// The content is entirely in those four `RangeTable`s, which are **script** properties out of
// Go's own Unicode tables. Rust's std has no script API, and `unicode-general-category` — already
// a dependency of mm-model, for the `unicode.IsLetter` gap — answers *general categories*, which
// partition the codepoint space differently and cannot express "is Han". A third-party script
// crate would be built against whatever Unicode version its author vendored, which is not
// necessarily the one the pinned Go toolchain carries. So the ranges are emitted from Go.
//
// Two things this makes measurable rather than assumed:
//
//   - **What is NOT CJK.** The CJK punctuation a reader would call Chinese — 。、「」— is
//     `Po`/`Ps`/`Pe` in the Common script and matches none of the four tables. So does the
//     ideographic space U+3000, the katakana middle dot U+30FB and the prolonged sound mark
//     U+30FC, and so do the fullwidth Latin forms. `ContainsCJK("你好。")` is true because of the
//     first two runes and would be false for the period alone.
//   - **The ranges are not all solid.** Han's U+3005..U+3007 has **stride 2**, so 々 and 〇 are
//     Han and 〆 between them is not. Katakana has a range of stride 288 and Hiragana one of
//     stride 30. An implementation that treats a range as an interval gets all three wrong, and
//     three of the annotations in this file's hand-picked list said the opposite until the
//     generator was run.
//   - **Hangul is not Hangul Syllables alone.** The script covers Jamo (U+1100), the compatibility
//     block (U+3131) and the syllables (U+AC00) in separate ranges, with gaps between them that
//     belong to other scripts.
//
// The boundary sweep is derived from the tables rather than hand-listed: every range contributes
// probes at lo-1, lo, lo+1, hi-1, hi and hi+1, so each edge is asserted from both sides. Hand
// listing them would test the ranges someone already believed in.
//
// **The Unicode version is the toolchain's, not the pinned tree's.** `unicode.Version` is
// recorded so a Go upgrade that moves a script boundary fails a Rust test instead of silently
// changing the answer. See [D-070].
//
// Determinism: every input derives from the tables. No rand, no time.Now — see [D-032].

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"unicode"

	"github.com/mattermost/mattermost/server/public/model"
)

// cjkScripts is the four tables in the order ContainsCJK tests them. The order does not change
// the answer — the tests are ORed — but it is the order the Rust port reproduces, and the order
// the emitted file lists them in.
var cjkScripts = []struct {
	name  string
	table *unicode.RangeTable
}{
	{"Han", unicode.Han},
	{"Hiragana", unicode.Hiragana},
	{"Katakana", unicode.Katakana},
	{"Hangul", unicode.Hangul},
}

// cjkRange is one Go `Range16`/`Range32` flattened to a single representation. Stride is carried
// rather than expanded: a stride greater than one over a wide range would explode into thousands
// of entries, and Go's own membership test is `lo <= r <= hi && (r-lo)%stride == 0`.
type cjkRange struct {
	Lo     uint32 `json:"lo"`
	Hi     uint32 `json:"hi"`
	Stride uint32 `json:"stride"`
}

// flattenRangeTable concatenates R16 and R32. Go stores them separately only to keep the common
// case in 16 bits; both slices are sorted ascending and every R16 entry is below every R32 entry,
// so the concatenation is sorted and a single search over it is equivalent to Go's two-step
// `Is`.
func flattenRangeTable(t *unicode.RangeTable) []cjkRange {
	out := make([]cjkRange, 0, len(t.R16)+len(t.R32))
	for _, r := range t.R16 {
		out = append(out, cjkRange{Lo: uint32(r.Lo), Hi: uint32(r.Hi), Stride: uint32(r.Stride)})
	}
	for _, r := range t.R32 {
		out = append(out, cjkRange{Lo: r.Lo, Hi: r.Hi, Stride: r.Stride})
	}
	return out
}

func writeUnicodeBehaviourFixture(outDir string) error {
	out := map[string]any{
		"unicode_version": unicode.Version,
		"tables":          cjkTablesAll(),
		"codepoints":      cjkCodepointsAll(),
		"strings":         cjkStringsAll(),
	}

	blob, err := json.MarshalIndent(out, "", "    ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(outDir, "behaviour_unicode.json"), append(blob, '\n'), 0o644)
}

// --- the tables --------------------------------------------------------------------------------

// cjkTablesAll records the shape of each table alongside the emitted Rust source, so a Rust test
// can assert the two agree without parsing the generated file.
func cjkTablesAll() []map[string]any {
	var res []map[string]any
	for _, s := range cjkScripts {
		ranges := flattenRangeTable(s.table)

		// The total number of codepoints the table admits, stride included. A single number that
		// a transcription error cannot survive.
		var count uint64
		maxStride := uint32(1)
		for _, r := range ranges {
			count += uint64((r.Hi-r.Lo)/r.Stride) + 1
			if r.Stride > maxStride {
				maxStride = r.Stride
			}
		}

		res = append(res, map[string]any{
			"name":            s.name,
			"ranges":          ranges,
			"range_count":     len(ranges),
			"codepoint_count": count,
			"max_stride":      maxStride,
			"r16_count":       len(s.table.R16),
			"r32_count":       len(s.table.R32),
		})
	}
	return res
}

// --- the codepoint sweep -------------------------------------------------------------------------

// cjkCodepointsAll probes every range edge from both sides, plus a hand-picked set for the cases
// a reader is most likely to get wrong. Each row records `ContainsCJK` on the single-rune string
// AND the four individual script tests, so a disagreement points at which table is wrong.
func cjkCodepointsAll() []map[string]any {
	seen := map[rune]bool{}
	var points []rune

	add := func(r rune) {
		// Surrogates are not valid runes: `string(r)` yields U+FFFD for them, which would make
		// the row a probe of something else entirely.
		if r < 0 || r > unicode.MaxRune || (r >= 0xD800 && r <= 0xDFFF) || seen[r] {
			return
		}
		seen[r] = true
		points = append(points, r)
	}

	for _, s := range cjkScripts {
		for _, rng := range flattenRangeTable(s.table) {
			for _, edge := range []int64{
				int64(rng.Lo) - 1, int64(rng.Lo), int64(rng.Lo) + 1,
				int64(rng.Hi) - 1, int64(rng.Hi), int64(rng.Hi) + 1,
			} {
				add(rune(edge))
			}
			// A stride greater than one means the range is not solid, so the second element and
			// the gap between the first two both have to be probed.
			if rng.Stride > 1 {
				add(rune(rng.Lo + 1))
				add(rune(rng.Lo + rng.Stride))
			}
		}
	}

	// Hand-picked: the ones a reader would place on the wrong side of the line.
	for _, r := range []rune{
		0x0000, 0x0041, 0x007A, 0x00FF, // ASCII and Latin-1
		0x3000,         // ideographic space — Common, not Han
		0x3001, 0x3002, // 、 and 。 — CJK punctuation, Common
		0x300C, 0x300D, // 「 」 — Common
		// U+3005..U+3007 is a Han range with STRIDE 2, so the two ends are Han and the middle
		// is not. Measured, and the opposite of what these three look like: 々 and 〇 read as
		// punctuation and a digit, and both are Han; 〆 U+3006 reads like its neighbours and is
		// not in any script table.
		0x3005, 0x3006, 0x3007,
		0x303B,         // 〻 — Han, in the U+3038..U+303B range
		0x30FB,         // ・ katakana middle dot — Common, NOT Katakana
		0x30FC,         // ー prolonged sound mark — Common, NOT Katakana
		0x3099, 0x309A, // combining voiced marks — Inherited
		0x309B, 0x309C, // spacing voiced marks — Common
		0x309D, 0x309E, // hiragana iteration marks
		0x30FD, 0x30FE, // katakana iteration marks
		0x3041, 0x3096, 0x309F, // hiragana block edges
		0x30A1, 0x30FA, 0x30FF, // katakana block edges
		0x31F0, 0x31FF, // katakana phonetic extensions
		0xFF66, 0xFF9D, // halfwidth katakana
		0xFF9E, 0xFF9F, // halfwidth voiced marks — Common
		0xFF21, 0xFF41, // fullwidth Latin — Common, not Han
		0x1100, 0x11FF, // Hangul Jamo
		0x3131, 0x318E, // Hangul compatibility Jamo
		0xAC00, 0xD7A3, // Hangul syllables
		0xD7B0, 0xD7FB, // Hangul Jamo extended-B
		0x4E00, 0x9FFF, // CJK unified ideographs
		0x3400, 0x4DBF, // extension A
		0xF900, 0xFAFF, // compatibility ideographs
		0x20000, 0x2A6DF, // extension B
		0x2A700, 0x2EBEF, // extensions C–F
		0x30000, 0x3134A, // extension G/H
		0x1B000, 0x1B001, // Kana supplement — Katakana then Hiragana
		0x1B164, 0x1B167, // small kana extension
		0x1F600, 0x1F1E6, // emoji — not CJK
		0x0E01, 0x0905, // Thai and Devanagari — not CJK
		0x10FFFF, // the last codepoint
	} {
		add(r)
	}

	sort.Slice(points, func(i, j int) bool { return points[i] < points[j] })

	var res []map[string]any
	for _, r := range points {
		row := map[string]any{
			"cp":  int64(r),
			"hex": fmt.Sprintf("U+%04X", r),
		}
		probe(row, func() {
			row["han"] = unicode.Is(unicode.Han, r)
			row["hiragana"] = unicode.Is(unicode.Hiragana, r)
			row["katakana"] = unicode.Is(unicode.Katakana, r)
			row["hangul"] = unicode.Is(unicode.Hangul, r)
			row["contains_cjk"] = model.ContainsCJK(string(r))
		})
		res = append(res, row)
	}
	return res
}

// --- whole strings -----------------------------------------------------------------------------

// cjkStringsAll is the function rather than the tables: the empty string, the early return, and
// the mixed cases where one rune out of many decides the answer.
func cjkStringsAll() []map[string]any {
	corpus := []struct{ name, in string }{
		{"empty", ""},
		{"ascii", "hello world"},
		{"ascii_digits", "0123456789"},
		{"han_only", "你好"},
		{"hiragana_only", "ひらがな"},
		{"katakana_only", "カタカナ"},
		{"hangul_only", "한국어"},
		{"hangul_jamo_only", "가"},
		{"hangul_compat_jamo", "ㄱ"},
		// Punctuation that reads as CJK and is not in any of the four scripts.
		{"cjk_punctuation_only", "。、「」〜"},
		{"ideographic_space_only", "　"},
		{"iteration_mark_only", "々"},
		{"katakana_middle_dot_only", "・ー"},
		{"fullwidth_latin_only", "ＡＢＣ"},
		// One CJK rune among many that are not: the loop returns on the first hit.
		{"leading_cjk", "日 followed by ascii"},
		{"trailing_cjk", "ascii followed by 日"},
		{"cjk_in_the_middle", "a日b"},
		{"punctuation_then_han", "。你"},
		// Emoji and other non-CJK scripts.
		{"emoji_only", "🙂🎌"},
		{"thai_only", "สวัสดี"},
		{"cyrillic_only", "привет"},
		// Astral-plane Han, so the loop sees a 4-byte rune.
		{"extension_b", "\U00020000"},
		{"extension_g", "\U00030000"},
		// Kana supplement, also astral.
		{"kana_supplement", "\U0001B000\U0001B001"},
		// A realistic mixed message.
		{"mixed_sentence", "Meeting at 3pm 会議室 A"},
		{"newlines_and_tabs", "a\n\tb"},
	}

	var res []map[string]any
	for _, c := range corpus {
		row := map[string]any{"name": c.name, "in": c.in}
		probe(row, func() {
			row["out"] = model.ContainsCJK(c.in)
			row["runes"] = len([]rune(c.in))
			row["bytes"] = len(c.in)
		})
		res = append(res, row)
	}
	return res
}

// --- the emitted Rust table ----------------------------------------------------------------------

// writeCjkScriptTable emits crates/mm-model/src/unicode_generated.rs. Not a fixture: Rust source,
// the same treatment emoji_data.go gets and for the same reason — the alternative is a human
// transcribing several hundred hexadecimal boundaries.
func writeCjkScriptTable(rustOutDir string) error {
	var b strings.Builder
	b.WriteString("//! @generated by `reference/dump` from Go's `unicode` package.\n")
	b.WriteString("//! DO NOT EDIT — re-run `cd reference/dump && go run .` instead.\n")
	b.WriteString("//!\n")
	b.WriteString("//! The four script `RangeTable`s `model.ContainsCJK` (unicode.go:8) tests. Rust's std has\n")
	b.WriteString("//! no script API, and `unicode-general-category` answers a different question, so these\n")
	b.WriteString("//! are emitted from Go rather than taken from a crate whose Unicode version may differ\n")
	b.WriteString("//! from the toolchain's.\n")
	b.WriteString("//!\n")
	fmt.Fprintf(&b, "//! Unicode version: %s. See [D-070] — this is the Go toolchain's, not the pinned tree's.\n", unicode.Version)
	b.WriteString("//!\n")
	b.WriteString("//! Each entry is `(lo, hi, stride)`, sorted ascending and non-overlapping, with Go's\n")
	b.WriteString("//! `R16` and `R32` concatenated — every `R16` bound is below every `R32` bound, so one\n")
	b.WriteString("//! search over the whole slice is equivalent to Go's two-step `Is`. A stride above 1\n")
	b.WriteString("//! means the range is not solid: membership is `lo <= r <= hi && (r - lo) % stride == 0`.\n")
	b.WriteString("\n")

	for _, s := range cjkScripts {
		ranges := flattenRangeTable(s.table)
		fmt.Fprintf(&b, "/// Go's `unicode.%s`, %d ranges.\n", s.name, len(ranges))
		b.WriteString("#[rustfmt::skip]\n")
		fmt.Fprintf(&b, "pub(crate) static %s: [(u32, u32, u32); %d] = [\n",
			strings.ToUpper(s.name), len(ranges))
		for _, r := range ranges {
			fmt.Fprintf(&b, "    (0x%04X, 0x%04X, %d),\n", r.Lo, r.Hi, r.Stride)
		}
		b.WriteString("];\n\n")
	}

	// The version the tables were generated against, so the Rust side can assert it against the
	// fixture and fail on a silent Go upgrade.
	fmt.Fprintf(&b, "/// The Unicode version Go's tables carried when this file was emitted.\n")
	fmt.Fprintf(&b, "pub(crate) const UNICODE_VERSION: &str = %q;\n", unicode.Version)

	path := filepath.Join(rustOutDir, "unicode_generated.rs")
	return os.WriteFile(path, []byte(b.String()), 0o644)
}
