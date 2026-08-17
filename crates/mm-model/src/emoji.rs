//! Port of `server/public/model/emoji.go`.
//!
//! The wire type is six fields; the substance is `IsValidEmojiName`, which rejects any name that
//! collides with a **system** emoji. That set lives in `emoji_data.go` — 4,473 lines of generated
//! map literal, 4,464 entries — so it is emitted from Go into
//! [`crate::emoji_generated`] rather than transcribed, the same call CLAUDE.md makes for
//! `permission.go`. Re-running `reference/dump` refreshes it.
//!
//! Getting that table wrong is not cosmetic: a missing entry lets a user create a custom emoji
//! the Go server refuses, and one the Go server would then shadow with its own.
//!
//! Pinned by `fixtures/emoji.json` and `fixtures/behaviour_emoji.json`.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::emoji_generated::SYSTEM_EMOJIS;
use crate::utils::{
    AppError, AppResult, get_millis, go_to_lower, is_valid_alpha_num_hyphen_underscore_plus,
    is_valid_id, new_id,
};

/// Port of `model.EmojiNameMaxLength` (emoji.go:14). Compared with `len()`, so **bytes**.
pub const EMOJI_NAME_MAX_LENGTH: usize = 64;

/// Port of `model.EmojiSortByName` (emoji.go:15).
pub const EMOJI_SORT_BY_NAME: &str = "name";

/// Port of `model.EmojiPattern` (emoji.go:18) — finds `:emoji_name:` references in message text.
///
/// **Unanchored**, unlike the validators: it is a scanner, not a matcher. Two consequences the
/// oracle pins: `::::` finds nothing (at least one character is required between the colons),
/// and overlapping references share their delimiter, so `:a:b:c:` yields `:a:` and `:c:` — the
/// middle name is swallowed because the leftmost match consumed its opening colon.
static EMOJI_PATTERN: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r":[a-zA-Z0-9_+-]+:").ok());

/// Every `:name:` reference in `text`, in order, delimiters included.
///
/// Fails **closed**: an uncompilable pattern yields no matches rather than panicking.
/// `regex_compiles` asserts that cannot happen.
pub fn find_emoji_references(text: &str) -> Vec<&str> {
    EMOJI_PATTERN
        .as_ref()
        .map(|re| re.find_iter(text).map(|m| m.as_str()).collect())
        .unwrap_or_default()
}

/// Port of `model.Emoji` (emoji.go:20).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Emoji {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    /// Length-checked but never validated — see [`Emoji::is_valid`].
    #[serde(rename = "creator_id")]
    pub creator_id: String,

    #[serde(rename = "name")]
    pub name: String,
}

impl Emoji {
    /// Port of `(*Emoji).IsValid` (emoji.go:72).
    ///
    /// Note what is *not* checked. `delete_at` is ignored entirely, and `creator_id` is only
    /// length-checked — `len(...) > 26`, in bytes, with no `IsValidId` call — so `"nope"` is an
    /// acceptable creator and an empty one is too. The same shape as `Channel.CreatorId`.
    ///
    /// The `id` and `creator_id` failures carry **no detail at all**, while the two timestamp
    /// failures carry `id=`. Clients parse these, so the asymmetry is reproduced.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("id", String::new()));
        }

        if self.create_at == 0 {
            return Err(err("create_at", format!("id={}", self.id)));
        }

        if self.update_at == 0 {
            return Err(err("update_at", format!("id={}", self.id)));
        }

        if self.creator_id.len() > 26 {
            return Err(err("user_id", String::new()));
        }

        is_valid_emoji_name(&self.name)
    }

    /// Port of `(*Emoji).PreSave` (emoji.go:103).
    ///
    /// Generates an `id` only when absent, lowercases `name` with Go's `strings.ToLower`
    /// semantics (see [`go_to_lower`] — `str::to_lowercase` is a different function), and
    /// **overwrites `create_at` unconditionally**, then copies it to `update_at`. Same
    /// `create_at` behaviour as `Team` and `Session`, opposite of `User` and `Channel`.
    ///
    /// It does not validate: `pre_save` will happily lowercase a name that [`Self::is_valid`]
    /// then rejects.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        self.name = go_to_lower(&self.name);
        self.create_at = get_millis();
        self.update_at = self.create_at;
    }
}

/// Port of `model.IsValidEmojiName` (emoji.go:92).
///
/// Two failures with **different error ids**, checked in order:
///
/// 1. `model.emoji.name.app_error` — empty, over 64 **bytes**, or containing anything outside
///    `[a-zA-Z0-9+_-]`.
/// 2. `model.emoji.system_emoji_name.app_error` — a name already taken by a system emoji.
///
/// The second is easy to underestimate: `a`, `+1` and `100` are all system emoji names, so a
/// perfectly ordinary-looking custom name can be rejected. Lookup is exact and case-sensitive —
/// `GRINNING` is free, `grinning` is not — but note [`Emoji::pre_save`] lowercases first.
pub fn is_valid_emoji_name(name: &str) -> AppResult {
    if name.is_empty()
        || name.len() > EMOJI_NAME_MAX_LENGTH
        || !is_valid_alpha_num_hyphen_underscore_plus(name)
    {
        return Err(err("name", String::new()));
    }

    if is_system_emoji_name(name) {
        return Err(err("system_emoji_name", String::new()));
    }

    Ok(())
}

/// Port of `model.IsSystemEmojiName` (emoji.go:40).
pub fn is_system_emoji_name(name: &str) -> bool {
    get_system_emoji_id(name).is_some()
}

/// Port of `model.GetSystemEmojiId` (emoji.go:45).
///
/// Go returns `(string, bool)` because a map miss and a genuinely empty value are
/// indistinguishable by indexing alone; `Option` carries the same information. Not every value
/// is a code-point sequence — `mattermost` maps to the literal string `mattermost`.
pub fn get_system_emoji_id(name: &str) -> Option<&'static str> {
    SYSTEM_EMOJIS
        .binary_search_by_key(&name, |(key, _)| *key)
        .ok()
        .map(|index| SYSTEM_EMOJIS[index].1)
}

/// Port of `reverseSystemEmojisMap` (emoji.go:62), built by `makeReverseEmojiMap`.
///
/// Go builds it in `init()`; this builds it on first use. Names are sorted, which Go achieves by
/// re-sorting on every append — the same result, without the quadratic behaviour.
static REVERSE_SYSTEM_EMOJIS: LazyLock<HashMap<&'static str, Vec<&'static str>>> =
    LazyLock::new(|| {
        let mut reverse: HashMap<&'static str, Vec<&'static str>> = HashMap::new();
        for (name, code) in SYSTEM_EMOJIS.iter() {
            reverse.entry(code).or_default().push(name);
        }
        // SYSTEM_EMOJIS is already sorted by name, so each bucket is built in sorted order and
        // needs no further work. Sorting anyway keeps the invariant local rather than inherited.
        for names in reverse.values_mut() {
            names.sort_unstable();
        }
        reverse
    });

/// Port of `model.GetEmojiNameFromUnicode` (emoji.go:64).
///
/// Returns the **alphabetically first** name for a code-point sequence, plus how many names
/// share it — several do: `1f1e8-1f1e6` has three. `("", 0)` when the sequence is unknown, and
/// the lookup is case-sensitive, so `1F600` misses where `1f600` hits.
pub fn get_emoji_name_from_unicode(unicode: &str) -> (&'static str, usize) {
    match REVERSE_SYSTEM_EMOJIS.get(unicode) {
        Some(names) => names.first().map_or(("", 0), |first| (first, names.len())),
        None => ("", 0),
    }
}

fn err(field: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "Emoji.IsValid",
        format!("model.emoji.{field}.app_error"),
        None,
        details,
        400,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn valid() -> Emoji {
        Emoji {
            id: "6bdz674pgq767e4jx75w4pf57a".into(),
            create_at: 1_700_000_000_000,
            update_at: 1_700_000_000_000,
            delete_at: 0,
            creator_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
            name: "custom_emoji".into(),
        }
    }

    #[test]
    fn regex_compiles() {
        assert!(EMOJI_PATTERN.is_some());
    }

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/emoji.json");
        let parsed: Emoji = serde_json::from_str(raw).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), original);
    }

    #[test]
    fn the_generated_table_is_sorted_and_searchable() {
        // binary_search_by_key is only correct on a sorted slice, and the table is generated —
        // so assert the property rather than trusting the generator.
        assert!(SYSTEM_EMOJIS.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(get_system_emoji_id("grinning"), Some("1f600"));
        assert_eq!(get_system_emoji_id("definitely_not_an_emoji"), None);
    }

    #[test]
    fn ordinary_looking_names_are_taken_by_system_emoji() {
        // The reason IsValidEmojiName has a second error id at all.
        for taken in ["a", "+1", "100", "grinning", "mattermost"] {
            assert!(
                is_system_emoji_name(taken),
                "{taken} should be a system emoji"
            );
        }
        assert!(!is_system_emoji_name("GRINNING"));
    }

    #[test]
    fn creator_id_is_length_checked_but_never_validated() {
        let mut e = valid();
        e.creator_id = "nope".into();
        e.is_valid().unwrap();

        e.creator_id = String::new();
        e.is_valid().unwrap();

        // The limit is bytes: 14 two-byte characters is 28 bytes.
        e.creator_id = "é".repeat(14);
        assert!(e.is_valid().is_err());
        e.creator_id = "é".repeat(13);
        e.is_valid().unwrap();
    }

    #[test]
    fn delete_at_is_never_checked() {
        let mut e = valid();
        e.delete_at = 1_700_000_000_000;
        e.is_valid().unwrap();
    }

    #[test]
    fn the_two_name_failures_have_different_ids() {
        assert_eq!(
            is_valid_emoji_name("has space").unwrap_err().id,
            "model.emoji.name.app_error"
        );
        assert_eq!(
            is_valid_emoji_name("grinning").unwrap_err().id,
            "model.emoji.system_emoji_name.app_error"
        );
    }

    #[test]
    fn pre_save_generates_an_id_only_when_absent() {
        let mut e = Emoji {
            name: "ALLCAPS".into(),
            create_at: 12345,
            ..Default::default()
        };
        e.pre_save();
        assert_eq!(e.id.len(), 26);
        assert_eq!(e.name, "allcaps");
        // create_at is overwritten unconditionally, and update_at follows it.
        assert_ne!(e.create_at, 12345);
        assert_eq!(e.create_at, e.update_at);

        let mut kept = valid();
        let id = kept.id.clone();
        kept.pre_save();
        assert_eq!(kept.id, id);
    }

    #[test]
    fn a_code_point_can_have_several_names() {
        let (name, count) = get_emoji_name_from_unicode("1f1e8-1f1e6");
        assert_eq!(name, "ca");
        assert!(count >= 2, "expected several names, got {count}");

        assert_eq!(get_emoji_name_from_unicode("1F600"), ("", 0));
        assert_eq!(get_emoji_name_from_unicode("nope"), ("", 0));
    }

    #[test]
    fn the_pattern_is_a_scanner_not_a_matcher() {
        assert_eq!(
            find_emoji_references("hello :smile: world"),
            vec![":smile:"]
        );
        // Overlapping references share their delimiter, so the middle one is swallowed.
        assert_eq!(find_emoji_references(":a:b:c:"), vec![":a:", ":c:"]);
        assert!(find_emoji_references("::::").is_empty());
    }
}

/// Parity tests driven by `fixtures/behaviour_emoji.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_emoji.json")).unwrap()
    }

    #[test]
    fn constants_match_go() {
        let oracle = oracle();
        let c = &oracle["constants"];
        assert_eq!(
            EMOJI_NAME_MAX_LENGTH as u64,
            c["name_max_length"].as_u64().unwrap()
        );
        assert_eq!(EMOJI_SORT_BY_NAME, c["sort_by_name"].as_str().unwrap());
        assert_eq!(
            EMOJI_PATTERN.as_ref().map(Regex::as_str),
            Some(c["pattern"].as_str().unwrap())
        );
    }

    /// The generated table is only as good as its size and contents; assert both against Go
    /// rather than trusting that the emitter ran.
    #[test]
    fn the_system_emoji_table_matches_go() {
        let oracle = oracle();
        assert_eq!(
            SYSTEM_EMOJIS.len() as u64,
            oracle["system_emoji_count"].as_u64().unwrap(),
            "emoji_generated.rs is stale; re-run reference/dump"
        );

        for (name, want) in oracle["system_emoji_sample"].as_object().unwrap() {
            let found = want["found"].as_bool().unwrap();
            let id = want["id"].as_str().unwrap();
            match get_system_emoji_id(name) {
                Some(got) => {
                    assert!(found, "case {name}: we found it, Go did not");
                    assert_eq!(got, id, "case {name}");
                }
                None => assert!(!found, "case {name}: we missed it, Go found {id}"),
            }
        }
    }

    #[test]
    fn is_system_emoji_name_matches_go() {
        let oracle = oracle();
        for (name, want) in oracle["is_system_emoji_name"].as_object().unwrap() {
            assert_eq!(
                is_system_emoji_name(name),
                want.as_bool().unwrap(),
                "name {name:?}"
            );
        }
    }

    #[test]
    fn get_emoji_name_from_unicode_matches_go() {
        let oracle = oracle();
        let cases = oracle["get_emoji_name_from_unicode"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let unicode = case["unicode"].as_str().unwrap();
            let (name, count) = get_emoji_name_from_unicode(unicode);
            assert_eq!(name, case["name"].as_str().unwrap(), "unicode {unicode:?}");
            assert_eq!(
                count as u64,
                case["count"].as_u64().unwrap(),
                "unicode {unicode:?}"
            );
        }
    }

    #[test]
    fn is_valid_emoji_name_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_valid_emoji_name"].as_object().unwrap();
        assert!(!cases.is_empty());
        for (name, want) in cases {
            let want = want.as_str().unwrap();
            match is_valid_emoji_name(name) {
                Ok(()) => assert!(want.is_empty(), "name {name:?}: valid, Go returned {want}"),
                Err(e) => assert_eq!(e.id, want, "name {name:?}"),
            }
        }
    }

    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_valid"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let emoji: Emoji = serde_json::from_value(case["emoji"].clone()).unwrap();
            let want = case["error_id"].as_str().unwrap();
            match emoji.is_valid() {
                Ok(()) => assert!(want.is_empty(), "case {name}: valid, Go returned {want}"),
                Err(e) => {
                    assert_eq!(e.id, want, "case {name}");
                    assert_eq!(
                        e.detailed_error,
                        case["detailed"].as_str().unwrap(),
                        "case {name}"
                    );
                    assert_eq!(e.status_code, 400, "case {name}");
                }
            }
        }
    }

    /// `PreSave` reads the clock and `NewId`, so the fixture records the invariants that hold
    /// regardless: the lowercased name, whether an id was kept or minted, and the timestamps.
    #[test]
    fn pre_save_matches_go() {
        let oracle = oracle();
        let cases = oracle["pre_save"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let in_id = case["in_id"].as_str().unwrap();

            let mut emoji = Emoji {
                id: in_id.to_string(),
                create_at: 12345,
                update_at: 67890,
                name: case["in_name"].as_str().unwrap().to_string(),
                ..Default::default()
            };
            emoji.pre_save();

            assert_eq!(
                emoji.name,
                case["out_name"].as_str().unwrap(),
                "case {name}"
            );
            assert_eq!(
                !in_id.is_empty() && emoji.id == in_id,
                case["id_preserved"].as_bool().unwrap(),
                "case {name}"
            );
            assert_eq!(
                in_id.is_empty() && emoji.id.len() == 26,
                case["id_generated"].as_bool().unwrap(),
                "case {name}"
            );
            assert_eq!(
                emoji.create_at == emoji.update_at,
                case["times_are_equal"].as_bool().unwrap(),
                "case {name}"
            );
            assert_eq!(
                emoji.create_at != 12345,
                case["create_at_overwritten"].as_bool().unwrap(),
                "case {name}"
            );
        }
    }

    #[test]
    fn emoji_pattern_matches_go() {
        let oracle = oracle();
        let cases = oracle["emoji_pattern"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let input = case["in"].as_str().unwrap();
            let want: Vec<&str> = case["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m.as_str().unwrap())
                .collect();
            assert_eq!(find_emoji_references(input), want, "input {input:?}");
        }
    }
}
