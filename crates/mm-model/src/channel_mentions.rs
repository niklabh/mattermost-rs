//! Port of `server/public/model/channel_mentions.go` (96 lines) — **whole file**.
//!
//! Three functions that pull `~channel-name` mentions out of text, plus the three `Post` methods
//! in post.go that wrap them (ported here rather than in `post.rs` so the whole surface reads in
//! one place; `post.rs` re-exports nothing because the methods live on `Post` via this module's
//! `impl` block).
//!
//! # The one thing that does not transcribe
//!
//! Go's pattern is ``\B~[a-zA-Z0-9\-_]+``. Copying that string into [`regex::Regex`] compiles and
//! is **wrong**: Go's RE2 defines `\b` and `\B` over the ASCII word class `[0-9A-Za-z_]`, while
//! the `regex` crate defines them over Unicode. So `é~chan` is a `\B` position in Go — `é` is not
//! an ASCII word character, so there is no boundary — and a word *boundary* in Rust, where `é` is
//! a letter. Go finds the mention; the naive port finds nothing.
//!
//! The fix is `(?-u:\B)`, and the evidence is a 164-codepoint sweep run through Go: the set of
//! characters that suppress a following mention is **exactly** `[0-9A-Za-z_]`, and not one of the
//! 36 non-ASCII probes — `é`, `日`, `٣` (an Arabic-Indic digit), `ｃ` (fullwidth `c`), `😀`, a
//! combining acute — is in it. Every one of those would have diverged.
//!
//! The character class has the same shape of trap and is likewise ASCII-only: the sweep confirms
//! a name is built from `[-0-9A-Za-z_]` and stops at `ｃ` and `３` as surely as at `.`.
//!
//! # The rest
//!
//! - **A name is the match minus its leading `~`** (Go: `match[1:]`), so `~town-square` yields
//!   `town-square`, never `~town-square`.
//! - **Dedup is global and order is first appearance.** All three functions allocate the seen-set
//!   once, outside every loop, so a name repeated in a later string or a later attachment is
//!   dropped rather than re-emitted. Comparison is byte equality, so `~Chan` and `~chan` are two
//!   mentions.
//! - **Nothing matched is a nil slice in Go**, not an empty one; it marshals as `null`. Rust has
//!   no such distinction for a `Vec`, and no caller marshals the result — see [D-061].
//! - **[`channel_mentions_from_attachments`] skips titles.** It reads `pretext`, `text` and field
//!   *values*; Go's comment says titles are labels. That puts it at odds with
//!   [`Post::channel_mentions_all`], which reaches attachments through `AllStrings` and **does**
//!   read both titles. The two functions genuinely disagree; both are pinned.
//! - **A non-string field value is skipped, not stringified.** `{"value": 42}` contributes
//!   nothing.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;

use crate::message_attachment::MessageAttachment;
use crate::post::{AllStringsOptions, Post};

/// Port of `model.channelMentionRegexp` (channel_mentions.go:11). Go: ``\B~[a-zA-Z0-9\-_]+``.
///
/// `(?-u:\B)` — **not** a bare `\B`. See the module docs; this is the whole reason the file needs
/// an oracle rather than a transcription.
static CHANNEL_MENTION_REGEX: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"(?-u:\B)~[a-zA-Z0-9\-_]+").ok());

/// Appends every mention in `s` that is not already in `seen`, preserving first-appearance order.
///
/// Go writes this loop out four times (once in `ChannelMentionsFromStrings`, three times in
/// `ChannelMentionsFromAttachments`); it is one function here because the four copies are
/// byte-identical.
fn append_mentions(s: &str, seen: &mut HashSet<String>, names: &mut Vec<String>) {
    let Some(re) = CHANNEL_MENTION_REGEX.as_ref() else {
        return;
    };

    for m in re.find_iter(s) {
        // Go's `match[1:]` cannot fail — the pattern starts with a literal `~`.
        let name = m.as_str().strip_prefix('~').unwrap_or(m.as_str());
        if !seen.contains(name) {
            names.push(name.to_string());
            seen.insert(name.to_string());
        }
    }
}

/// Port of `model.ChannelMentions` (channel_mentions.go:13).
pub fn channel_mentions(message: &str) -> Vec<String> {
    channel_mentions_from_strings(std::slice::from_ref(&message))
}

/// Port of `model.ChannelMentionsFromStrings` (channel_mentions.go:20).
///
/// Deduplicates across **all** inputs, not per string. Callers typically pass
/// [`Post::all_strings`] so mentions are found in the message, the attachments and the
/// interactive payloads consistently.
pub fn channel_mentions_from_strings<S: AsRef<str>>(strs: &[S]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();

    for s in strs {
        let s = s.as_ref();
        // Go's short circuit. Redundant — a match requires a `~` — but reproduced because it is
        // the reason a tilde-free string costs nothing.
        if !s.contains('~') {
            continue;
        }
        append_mentions(s, &mut seen, &mut names);
    }

    names
}

/// Port of `model.ChannelMentionsFromAttachments` (channel_mentions.go:42).
///
/// Scans `pretext`, `text` and field **values** — not titles, and not `fallback`, `author_name`
/// or `footer` either. [`Post::channel_mentions_all`] reads all of those, so the two disagree
/// about the same attachment; see the module docs.
pub fn channel_mentions_from_attachments(attachments: &[MessageAttachment]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();

    for attachment in attachments {
        append_mentions(&attachment.pretext, &mut seen, &mut names);
        append_mentions(&attachment.text, &mut seen, &mut names);

        for field in attachment.fields.iter().flatten() {
            // Go: `switch v := field.Value.(type) { case string: …; default: continue }`. A
            // number, a bool, a null, an array and an object are all skipped outright — the
            // value is never rendered to a string first.
            let Some(value) = field.value.as_str() else {
                continue;
            };
            append_mentions(value, &mut seen, &mut names);
        }
    }

    names
}

impl Post {
    /// Port of `(*Post).ChannelMentions` (post.go:1155). The message alone — no attachments, no
    /// interactive payloads.
    pub fn channel_mentions(&self) -> Vec<String> {
        channel_mentions(&self.message)
    }

    /// Port of `(*Post).ChannelMentionsAll` (post.go:1162), deprecated in Go in favour of
    /// [`Self::channel_mentions_all_with_options`].
    ///
    /// **Its doc comment contradicts its body.** The comment says "interactive blocks are
    /// omitted"; the call passes `OmitInteractiveBlocks: false`, which *includes* them. The body
    /// is what is ported, and the oracle records both option values for every corpus post so the
    /// two cannot be confused — `mm_blocks`, `blocks` and `cards` all reach the result here.
    pub fn channel_mentions_all(&self) -> Vec<String> {
        channel_mentions_from_strings(&self.all_strings(AllStringsOptions {
            omit_interactive_blocks: false,
        }))
    }

    /// Port of `(*Post).ChannelMentionsAllWithOptions` (post.go:1169).
    ///
    /// Used by `FillInPostProps` to populate the `channel_mentions` prop for rendering.
    pub fn channel_mentions_all_with_options(&self, opts: AllStringsOptions) -> Vec<String> {
        channel_mentions_from_strings(&self.all_strings(opts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_drops_the_leading_tilde() {
        assert_eq!(channel_mentions(" ~town-square"), ["town-square"]);
    }

    #[test]
    fn a_mention_glued_to_a_word_is_not_found() {
        // \B: the position before `~` must NOT be an ASCII word boundary.
        for blocked in ["a~chan", "1~chan", "_~chan"] {
            assert!(channel_mentions(blocked).is_empty(), "{blocked}");
        }
        // `-` and `.` are not word characters, so these DO match.
        for found in ["-~chan", ".~chan", "(~chan", "~chan", "~~chan"] {
            assert_eq!(channel_mentions(found), ["chan"], "{found}");
        }
    }

    /// The whole reason this file has an oracle. A bare `\B` would fail every one of these.
    #[test]
    fn a_non_ascii_neighbour_does_not_block_the_mention() {
        for s in [
            "é~chan",
            "日~chan",
            "😀~chan",
            "٣~chan",
            "ｃ~chan",
            "\u{0301}~chan",
        ] {
            assert_eq!(channel_mentions(s), ["chan"], "{s}");
        }
    }

    #[test]
    fn the_name_class_is_ascii_only() {
        assert_eq!(channel_mentions(" ~aZ09-_"), ["aZ09-_"]);
        // Fullwidth `c` and an Arabic-Indic digit both terminate the name.
        assert_eq!(channel_mentions(" ~chｃan"), ["ch"]);
        assert_eq!(channel_mentions(" ~ch٣an"), ["ch"]);
    }

    #[test]
    fn dedup_is_global_and_order_is_first_appearance() {
        assert_eq!(
            channel_mentions_from_strings(&[" ~z ~y ~x ~z", " ~y ~w"]),
            ["z", "y", "x", "w"]
        );
        // Byte equality, so case matters.
        assert_eq!(channel_mentions(" ~Chan ~chan"), ["Chan", "chan"]);
    }
}

/// Parity tests driven by `fixtures/behaviour_channel_mentions.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_channel_mentions.json"
        ))
        .unwrap()
    }

    /// Go records a nil slice as JSON `null` and an empty one as `[]`; all three functions return
    /// nil when nothing matched, so `null` and `[]` both mean "no names" to us. See [D-061].
    fn expected(v: &Value) -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .map(|s| s.as_str().unwrap().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// The measurement the module exists for: one codepoint driven through four positions in the
    /// pattern, 164 times. A bare `\B` fails the `prefix` column on all 36 non-ASCII rows; a
    /// Unicode-aware character class fails `first`, `middle` and `suffix`.
    #[test]
    fn the_regex_matches_go_over_the_codepoint_sweep() {
        let oracle = oracle();
        let rows = oracle["sweep"].as_array().unwrap();
        assert_eq!(rows.len(), 164, "sweep shrank");

        let mut non_ascii = 0;
        for row in rows {
            let cp = u32::try_from(row["codepoint"].as_i64().unwrap()).unwrap();
            let c = char::from_u32(cp).unwrap();
            let label = format!("U+{cp:04X}");

            assert_eq!(
                channel_mentions(&format!("{c}~chan")),
                expected(&row["prefix"]),
                "{label}: prefix"
            );
            assert_eq!(
                channel_mentions(&format!(" ~{c}")),
                expected(&row["first"]),
                "{label}: first"
            );
            assert_eq!(
                channel_mentions(&format!(" ~ch{c}an")),
                expected(&row["middle"]),
                "{label}: middle"
            );
            assert_eq!(
                channel_mentions(&format!(" ~chan{c}")),
                expected(&row["suffix"]),
                "{label}: suffix"
            );

            if cp > 127 {
                non_ascii += 1;
            }
        }
        assert!(non_ascii >= 30, "the sweep lost its non-ASCII half");
    }

    /// Stated as its own assertion rather than left implicit in the sweep: the set of characters
    /// that suppress a following mention is exactly the **ASCII** word class. If a future edit
    /// drops the `(?-u:…)` this fails with a readable message instead of 36 sweep rows.
    #[test]
    fn only_ascii_word_characters_suppress_a_mention() {
        let oracle = oracle();
        let blocked: Vec<u32> = oracle["sweep"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["prefix"].as_array().is_none_or(|a| a.is_empty()))
            .map(|r| u32::try_from(r["codepoint"].as_i64().unwrap()).unwrap())
            .collect();

        let expected: Vec<u32> = (0u32..128)
            .filter(|c| {
                let c = char::from_u32(*c).unwrap();
                c.is_ascii_alphanumeric() || c == '_'
            })
            .collect();
        assert_eq!(blocked, expected, "Go's blocking set");

        // Asserted against *our* regex too, over the same alphabet — otherwise this test only
        // describes the fixture and stays green while the port diverges.
        let ours: Vec<u32> = oracle["sweep"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| u32::try_from(r["codepoint"].as_i64().unwrap()).unwrap())
            .filter(|cp| {
                let c = char::from_u32(*cp).unwrap();
                channel_mentions(&format!("{c}~chan")).is_empty()
            })
            .collect();
        assert_eq!(ours, expected, "our blocking set");
    }

    #[test]
    fn from_strings_matches_go() {
        let oracle = oracle();
        let cases = oracle["from_strings"].as_array().unwrap();
        assert!(cases.len() > 40, "corpus shrank: {}", cases.len());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input: Vec<String> = case["in"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|s| s.as_str().unwrap().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            assert_eq!(
                channel_mentions_from_strings(&input),
                expected(&case["out"]),
                "{name}"
            );
        }
    }

    #[test]
    fn channel_mentions_matches_go() {
        let oracle = oracle();
        let cases = oracle["single"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let message = case["in"].as_array().unwrap()[0].as_str().unwrap();
            assert_eq!(
                channel_mentions(message),
                expected(&case["out"]),
                "{message:?}"
            );
        }
    }

    #[test]
    fn from_attachments_matches_go() {
        let oracle = oracle();
        let cases = oracle["from_attachments"].as_array().unwrap();
        assert!(!cases.is_empty());

        let mut checked = 0;
        let mut skipped = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            // A nil attachment or a nil field has no Rust spelling — see [D-033].
            if case["has_nil_element"].as_bool().unwrap() {
                skipped += 1;
                continue;
            }

            // Go marshals a nil slice as `null`; it and `[]` both mean "no attachments".
            let attachments: Vec<MessageAttachment> = match &case["in"] {
                Value::Null => Vec::new(),
                doc => {
                    serde_json::from_value(doc.clone()).unwrap_or_else(|e| panic!("{name}: {e}"))
                }
            };
            assert_eq!(
                channel_mentions_from_attachments(&attachments),
                expected(&case["out"]),
                "{name}"
            );
            checked += 1;
        }
        assert_eq!(skipped, 2, "the nil-element cases went missing");
        assert!(checked > 15, "corpus shrank: {checked}");
    }

    #[test]
    fn the_post_methods_match_go() {
        let oracle = oracle();
        let cases = oracle["post_methods"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let post: Post = serde_json::from_value(case["post"].clone()).unwrap();

            assert_eq!(
                post.channel_mentions(),
                expected(&case["message"]),
                "{name}: message"
            );
            assert_eq!(
                post.channel_mentions_all(),
                expected(&case["all"]),
                "{name}: all"
            );
            assert_eq!(
                post.channel_mentions_all_with_options(AllStringsOptions {
                    omit_interactive_blocks: false
                }),
                expected(&case["with_blocks"]),
                "{name}: with blocks"
            );
            assert_eq!(
                post.channel_mentions_all_with_options(AllStringsOptions {
                    omit_interactive_blocks: true
                }),
                expected(&case["without_blocks"]),
                "{name}: without blocks"
            );
        }
    }

    /// The deprecated wrapper's doc comment says interactive blocks are omitted and its body says
    /// otherwise. Pinned so a future reader "fixing" the discrepancy fails a test: at least one
    /// corpus post must have `channel_mentions_all` agree with the *including* option and differ
    /// from the omitting one.
    #[test]
    fn channel_mentions_all_includes_interactive_blocks() {
        let oracle = oracle();
        let divergent = oracle["post_methods"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["all"] == c["with_blocks"] && c["all"] != c["without_blocks"])
            .count();
        assert!(divergent >= 4, "corpus lost its interactive-block cases");
    }

    /// The two attachment paths disagree about titles, which is easy to read as a bug in one of
    /// them. Both are Go's.
    #[test]
    fn the_two_attachment_paths_disagree_about_titles() {
        let attachment = MessageAttachment {
            title: " ~atitle".to_string(),
            text: " ~atext".to_string(),
            ..MessageAttachment::default()
        };
        // The dedicated function reads text and skips the title.
        assert_eq!(
            channel_mentions_from_attachments(std::slice::from_ref(&attachment)),
            ["atext"]
        );

        // Reached through AllStrings, the title is read as well.
        let post: Post = serde_json::from_str(
            r#"{"props":{"attachments":[{"title":" ~atitle","text":" ~atext"}]}}"#,
        )
        .unwrap();
        assert_eq!(post.channel_mentions_all(), ["atitle", "atext"]);
    }
}
