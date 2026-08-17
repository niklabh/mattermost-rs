//! Port of `model/unicode.go` (unicode.go:1–18) — **whole file**.
//!
//! One function and no types, so no wire format and no fixture in the plain registry:
//!
//! ```go
//! func ContainsCJK(s string) bool {
//!     for _, r := range s {
//!         if unicode.Is(unicode.Han, r) || unicode.Is(unicode.Hiragana, r) ||
//!             unicode.Is(unicode.Katakana, r) || unicode.Is(unicode.Hangul, r) {
//!             return true
//!         }
//!     }
//!     return false
//! }
//! ```
//!
//! # The function is four lines; the content is the four tables
//!
//! Those are **script** properties, and Rust's std has no script API. The crate already depends
//! on `unicode-general-category` — for the `unicode.IsLetter` gap in [`crate::utils`] — and it
//! cannot help here: general categories partition the codepoint space differently and there is no
//! category that means "Han". A third-party script crate would carry whatever Unicode version its
//! author vendored, which need not be the Go toolchain's.
//!
//! So the ranges are **emitted from Go** into [`crate::unicode_generated`], the same treatment
//! `emoji_data.go` gets. Nothing here is transcribed.
//!
//! # A range is not an interval
//!
//! Go's `RangeTable` entries carry a **stride**, and three of these four tables use it. All four
//! such entries, from the generated file:
//!
//! | script | range | stride | codepoints it admits |
//! |---|---|---|---|
//! | Han | `U+3005..U+3007` | 2 | 2 — 々 and 〇 are Han, 〆 between them is not |
//! | Hiragana | `U+1B132..U+1B150` | 30 | 2 |
//! | Katakana | `U+1B000..U+1B0E0` | 288 | 2 |
//! | Katakana | `U+1B155..U+1B164` | 15 | 2 |
//!
//! Membership is `lo <= r <= hi && (r - lo) % stride == 0`, which is what [`is_in`] implements.
//! Reading those four entries as solid intervals would admit **331** codepoints that are not in
//! any of the scripts — and would make 〆 Han. The hand-written annotations in this file's oracle
//! claimed exactly that about U+3005, U+3007 and U+303B until the generator was first run; the
//! table corrected all three.
//!
//! # What is not CJK
//!
//! Measured over 306 codepoints, because the intuitive answer is wrong in both directions: the
//! ideographic space U+3000, the punctuation 。、「」, the katakana middle dot U+30FB and the
//! prolonged sound mark U+30FC are all **Common** script and none of them is CJK — while the
//! iteration marks 々 U+3005, 〻 U+303B, ゝ U+309D and ヽ U+30FD all are.

use crate::unicode_generated::{HAN, HANGUL, HIRAGANA, KATAKANA};

/// The Unicode version the four script tables were generated against.
///
/// It is the **Go toolchain's**, not the pinned Mattermost tree's, so a `go` upgrade can move a
/// script boundary without anything in `reference/mattermost` changing. Exposed rather than kept
/// private because "which Unicode do these tables speak" is a deployment question — two servers
/// built against different Go releases can disagree about whether a newly assigned codepoint is
/// Han. See [D-070].
pub const CJK_UNICODE_VERSION: &str = crate::unicode_generated::UNICODE_VERSION;

/// Port of `model.ContainsCJK` (unicode.go:8).
///
/// True if any character is Han, Hiragana, Katakana or Hangul. The scripts are tested in Go's
/// order, which cannot change the answer — the tests are ORed — but keeps the port readable
/// against the source.
///
/// **One thing Go can do that this cannot.** Go's `for _, r := range s` yields `U+FFFD` for each
/// byte of invalid UTF-8, so `ContainsCJK` on a malformed string is defined there. A Rust `&str`
/// cannot hold invalid UTF-8, so the case is unreachable rather than divergent — and `U+FFFD` is
/// in none of the four tables, so a caller holding `&[u8]` can lossily convert without changing
/// the answer.
pub fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        let r = c as u32;
        is_in(&HAN, r) || is_in(&HIRAGANA, r) || is_in(&KATAKANA, r) || is_in(&HANGUL, r)
    })
}

/// Port of `unicode.Is` for one of the generated tables.
///
/// Go splits its search across `R16` and `R32` and picks a linear scan for small tables; the
/// generated slices concatenate the two, and both strategies answer the same question, so this
/// binary-searches the whole slice. The stride test is the part that is not optional — see the
/// module docs.
fn is_in(table: &[(u32, u32, u32)], r: u32) -> bool {
    // `partition_point` gives the first range whose `lo` exceeds `r`, so the candidate is the
    // one before it. The ranges are sorted and non-overlapping, so there is at most one.
    let index = table.partition_point(|&(lo, _, _)| lo <= r);
    let Some(&(lo, hi, stride)) = index.checked_sub(1).map(|i| &table[i]) else {
        return false;
    };
    r <= hi && (stride == 1 || (r - lo) % stride == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_string_contains_nothing() {
        assert!(!contains_cjk(""));
    }

    #[test]
    fn ascii_is_not_cjk() {
        assert!(!contains_cjk("hello world 0123456789"));
    }

    #[test]
    fn each_of_the_four_scripts_counts() {
        assert!(contains_cjk("你好"), "Han");
        assert!(contains_cjk("ひらがな"), "Hiragana");
        assert!(contains_cjk("カタカナ"), "Katakana");
        assert!(contains_cjk("한국어"), "Hangul");
    }

    /// One hit anywhere is enough, and the scan is left to right.
    #[test]
    fn one_character_decides_the_answer() {
        assert!(contains_cjk("a日b"));
        assert!(contains_cjk("日 followed by ascii"));
        assert!(contains_cjk("ascii followed by 日"));
    }

    /// The half that surprises: punctuation that looks Chinese and is not in any script table.
    #[test]
    fn cjk_punctuation_is_not_cjk() {
        for s in ["。、「」", "\u{3000}", "・ー", "ＡＢＣ"] {
            assert!(!contains_cjk(s), "{s:?}");
        }
    }

    /// ...and the half that surprises the other way.
    #[test]
    fn the_iteration_marks_are_cjk() {
        for s in ["々", "〻", "ゝ", "ヽ"] {
            assert!(contains_cjk(s), "{s:?}");
        }
    }

    /// Han's `U+3005..U+3007` has stride 2. An implementation that read it as an interval would
    /// call U+3006 Han.
    #[test]
    fn a_stride_leaves_a_hole_in_the_middle_of_a_range() {
        assert!(contains_cjk("\u{3005}"), "U+3005 is Han");
        assert!(!contains_cjk("\u{3006}"), "U+3006 is the hole");
        assert!(contains_cjk("\u{3007}"), "U+3007 is Han");
    }

    #[test]
    fn astral_plane_characters_are_reached() {
        assert!(contains_cjk("\u{20000}"), "extension B");
        assert!(contains_cjk("\u{30000}"), "extension G");
        assert!(contains_cjk("\u{1B000}"), "kana supplement");
    }

    #[test]
    fn other_scripts_and_emoji_are_not_cjk() {
        for s in ["🙂🎌", "สวัสดี", "привет", "Ωμέγα"] {
            assert!(!contains_cjk(s), "{s:?}");
        }
    }

    /// The generated tables must stay sorted and non-overlapping, because [`is_in`] binary
    /// searches them. The generator emits them that way; this fails if that ever stops being
    /// true rather than letting the search return a silently wrong answer.
    #[test]
    fn the_generated_tables_are_sorted_and_disjoint() {
        for (name, table) in [
            ("HAN", &HAN[..]),
            ("HIRAGANA", &HIRAGANA[..]),
            ("KATAKANA", &KATAKANA[..]),
            ("HANGUL", &HANGUL[..]),
        ] {
            assert!(!table.is_empty(), "{name}");
            for window in table.windows(2) {
                let (lo, hi, _) = window[0];
                let (next_lo, _, _) = window[1];
                assert!(lo <= hi, "{name}: {lo:#X}..{hi:#X} is inverted");
                assert!(hi < next_lo, "{name}: {hi:#X} overlaps {next_lo:#X}");
            }
            for &(lo, hi, stride) in table {
                assert!(stride >= 1, "{name}: {lo:#X} has stride 0");
                assert_eq!(
                    (hi - lo) % stride,
                    0,
                    "{name}: {lo:#X}..{hi:#X} does not land on its stride"
                );
            }
        }
    }
}

/// Parity tests driven by `fixtures/behaviour_unicode.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_unicode.json")).unwrap()
    }

    /// The tables come from the **Go toolchain's** Unicode version, not from the pinned
    /// Mattermost tree, so a `go` upgrade can move a script boundary without anything in
    /// `reference/mattermost` changing. Asserting the version turns that into a failing test with
    /// an obvious cause instead of a handful of mysterious codepoint failures. See [D-070].
    #[test]
    fn the_unicode_version_matches_the_generator() {
        let oracle = oracle();
        assert_eq!(
            oracle["unicode_version"].as_str().unwrap(),
            CJK_UNICODE_VERSION
        );
    }

    /// The emitted tables against Go's, entry for entry. This is the whole of the port: if the
    /// ranges are right, `contains_cjk` cannot be wrong for a reason the sweep would not catch.
    #[test]
    fn the_generated_tables_match_go() {
        let oracle = oracle();
        let cases = oracle["tables"].as_array().unwrap();
        assert_eq!(cases.len(), 4);

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let ours: &[(u32, u32, u32)] = match name {
                "Han" => &HAN,
                "Hiragana" => &HIRAGANA,
                "Katakana" => &KATAKANA,
                "Hangul" => &HANGUL,
                other => panic!("unexpected script {other}"),
            };

            let theirs = case["ranges"].as_array().unwrap();
            assert_eq!(ours.len(), theirs.len(), "{name}: range count");
            assert_eq!(
                ours.len(),
                case["range_count"].as_u64().unwrap() as usize,
                "{name}"
            );

            for (i, (want, &(lo, hi, stride))) in theirs.iter().zip(ours).enumerate() {
                assert_eq!(
                    u64::from(lo),
                    want["lo"].as_u64().unwrap(),
                    "{name}[{i}] lo"
                );
                assert_eq!(
                    u64::from(hi),
                    want["hi"].as_u64().unwrap(),
                    "{name}[{i}] hi"
                );
                assert_eq!(
                    u64::from(stride),
                    want["stride"].as_u64().unwrap(),
                    "{name}[{i}] stride"
                );
            }

            // A single number a transcription error cannot survive: how many codepoints the
            // table admits once the strides are taken into account.
            let counted: u64 = ours
                .iter()
                .map(|&(lo, hi, stride)| u64::from((hi - lo) / stride) + 1)
                .sum();
            assert_eq!(
                counted,
                case["codepoint_count"].as_u64().unwrap(),
                "{name}: codepoint count"
            );
        }
    }

    /// Every range edge from both sides, plus the hand-picked set. Each row carries the four
    /// individual script verdicts as well as `ContainsCJK`, so a failure names the table.
    #[test]
    fn the_codepoint_sweep_matches_go() {
        let oracle = oracle();
        let cases = oracle["codepoints"].as_array().unwrap();
        assert_eq!(cases.len(), 306, "the codepoint sweep changed size");

        let mut cjk = 0;
        for case in cases {
            assert!(!case["panicked"].as_bool().unwrap());

            let cp = case["cp"].as_u64().unwrap() as u32;
            let hex = case["hex"].as_str().unwrap();
            let ch = char::from_u32(cp).unwrap_or_else(|| panic!("{hex} is not a char"));

            for (script, table) in [
                ("han", &HAN[..]),
                ("hiragana", &HIRAGANA[..]),
                ("katakana", &KATAKANA[..]),
                ("hangul", &HANGUL[..]),
            ] {
                assert_eq!(
                    is_in(table, cp),
                    case[script].as_bool().unwrap(),
                    "{hex}: {script}"
                );
            }

            let want = case["contains_cjk"].as_bool().unwrap();
            assert_eq!(contains_cjk(&ch.to_string()), want, "{hex}");
            if want {
                cjk += 1;
            }
        }

        // The sweep is only worth anything if it straddles the boundary rather than sitting on
        // one side of it.
        assert!(
            cjk > 100 && cjk < cases.len() - 100,
            "{cjk} of {}",
            cases.len()
        );
    }

    #[test]
    fn the_string_corpus_matches_go() {
        let oracle = oracle();
        let cases = oracle["strings"].as_array().unwrap();
        assert_eq!(cases.len(), 26, "the string corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap());

            let input = case["in"].as_str().unwrap();
            assert_eq!(
                contains_cjk(input),
                case["out"].as_bool().unwrap(),
                "{name}: {input:?}"
            );

            // Go counts runes and bytes the way Rust counts chars and len(), which is worth
            // pinning here because the loop is over runes and the trap would be silent.
            assert_eq!(
                input.chars().count() as u64,
                case["runes"].as_u64().unwrap(),
                "{name}: rune count"
            );
            assert_eq!(
                input.len() as u64,
                case["bytes"].as_u64().unwrap(),
                "{name}: byte count"
            );
        }
    }
}
