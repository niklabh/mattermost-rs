//! Port of `model/file_info_search_results.go` (file_info_search_results.go:1–18) — **whole
//! file**.
//!
//! Eighteen lines: a `map[string][]string` alias, a struct, and a constructor that assigns two
//! fields.
//!
//! ```go
//! type FileInfoSearchResults struct {
//!     *FileInfoList
//!     Matches FileInfoSearchMatches `json:"matches"`
//! }
//! ```
//!
//! # It is `post_search_results.go` minus the methods, and that is the whole difference
//!
//! [`crate::post_search_results::PostSearchResults`] is the same declaration with different
//! nouns, and everything the embedded pointer costs on the wire is identical here. What is
//! *absent* is the part that made that file dangerous: there is no `ToJSON`, no `EncodeJSON`, no
//! `ForPlugin` and no `Auditable`, so none of the three nil-embed panics of [D-054] has a
//! counterpart, nothing strips action integrations, and [D-028] gains no entry. The port is a
//! type declaration and a constructor.
//!
//! # The embed flattens, and a nil embed drops five keys
//!
//! `order`, `file_infos` and the three scalars sit beside `matches` in one flat object — there is
//! no `file_info_list` key and no nesting. When the pointer is **nil**, `encoding/json` skips
//! every field whose index path runs through it, so the whole document is `{"matches":null}`
//! rather than five nulls plus matches. That state is not hypothetical:
//! [`FileInfoSearchResults::new`] takes the list from its caller and `&FileInfoSearchResults{}`
//! has it nil.
//!
//! # Which keys allocate the embed is the wire format
//!
//! Go allocates the embedded pointer lazily, the first time a decode walks into it. Measured:
//!
//! | document | embed after decode |
//! |---|---|
//! | `{}`, `{"matches":{}}`, `{"matches":null}`, `{"nope":1}` | nil |
//! | `{"order":null}`, `{"next_file_info_id":"n1"}`, `{"first_inaccessible_file_time":0}` | allocated |
//!
//! So the round trip of `{"matches":{}}` is `{"matches":{}}`, while the round trip of
//! `{"order":null}` gains five keys it did not have — and an explicitly-zero scalar allocates
//! just as an explicitly-null collection does. That is why [`FileInfoSearchResults`] carries a
//! hand-written [`Deserialize`] keyed on [`FileInfoList::WIRE_KEYS`]: serde's `flatten` on an
//! `Option` always produces `Some`, which would put those five keys on every response.
//!
//! One narrow difference from `PostList` here. That type has a `json:"-"` field, so it has a
//! promoted field that is nevertheless an *unknown* key; `FileInfoList` has none, so its key set
//! is simply all five of its fields.

use std::collections::BTreeMap;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::file_info_list::FileInfoList;
use crate::utils::StringArray;

/// Port of `model.FileInfoSearchMatches` (file_info_search_results.go:6) — Go's
/// `map[string][]string`.
///
/// A `BTreeMap` because Go sorts map keys byte-wise when marshalling and `String: Ord` is
/// byte-wise too ([D-027]). The value is an `Option` because Go's `[]string` is nillable and the
/// difference reaches the client: `{"f1":null}` round-trips as `null`, `{"f1":[]}` as `[]`.
pub type FileInfoSearchMatches = BTreeMap<String, Option<StringArray>>;

/// Port of `model.FileInfoSearchResults` (file_info_search_results.go:8).
///
/// `file_info_list` is an `Option` because Go's embed is a pointer that is nil for every document
/// carrying none of [`FileInfoList::WIRE_KEYS`] — see the module docs for what that costs on the
/// wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FileInfoSearchResults {
    /// The embedded `*FileInfoList`. Flattened, so its keys sit at the top level and precede
    /// `matches` — which is the order Go emits them in, promoted fields first.
    #[serde(flatten)]
    pub file_info_list: Option<FileInfoList>,

    /// No `omitempty`, so a nil map reaches the client as `null` rather than `{}`.
    #[serde(rename = "matches")]
    pub matches: Option<FileInfoSearchMatches>,
}

impl FileInfoSearchResults {
    /// Port of `model.MakeFileInfoSearchResults` (file_info_search_results.go:13).
    ///
    /// A plain field assignment in Go — and a **positional** one
    /// (`&FileInfoSearchResults{fileInfos, matches}`), so a field added upstream would fail to
    /// compile there rather than silently going unset.
    ///
    /// A nil list is accepted and is the state that drops five keys from the wire. Unlike
    /// `MakePostSearchResults` that is all it costs: no method here dereferences the embed, so
    /// there is nothing to crash later.
    pub fn new(
        file_info_list: Option<FileInfoList>,
        matches: Option<FileInfoSearchMatches>,
    ) -> Self {
        Self {
            file_info_list,
            matches,
        }
    }
}

/// Hand-written because Go decides the embed's nil-ness from **which keys are present**, and
/// serde's `flatten` cannot: `Option<T>` behind a flatten always deserialises as `Some`, which
/// would turn every `{"matches":…}` response into one carrying five extra keys.
///
/// Unknown keys are ignored, as Go does. One divergence, inherited: Go matches field names
/// case-insensitively, so `{"ORDER":[]}` allocates the embed *and* sets `order` there while it is
/// an unknown key here — [D-040], asserted rather than skipped.
impl<'de> Deserialize<'de> for FileInfoSearchResults {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ResultsVisitor;

        impl<'de> Visitor<'de> for ResultsVisitor {
            type Value = FileInfoSearchResults;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a file info search results object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<FileInfoSearchResults, A::Error>
            where
                A: MapAccess<'de>,
            {
                // Buffered rather than decoded field by field so `FileInfoList`'s own derive
                // stays the single definition of its wire format.
                let mut promoted = serde_json::Map::new();
                let mut matches = None;

                while let Some(key) = map.next_key::<String>()? {
                    if key == "matches" {
                        // A duplicate key takes the last value, as Go does.
                        matches = map.next_value::<Option<FileInfoSearchMatches>>()?;
                    } else if FileInfoList::WIRE_KEYS.contains(&key.as_str()) {
                        map.next_value::<serde_json::Value>()
                            .map(|value| promoted.insert(key, value))?;
                    } else {
                        map.next_value::<de::IgnoredAny>()?;
                    }
                }

                // Empty means no *recognised* key was present, which is exactly when Go leaves
                // the embedded pointer nil. An unknown key does not allocate it; an explicit
                // `null` or an explicitly-zero scalar does.
                let file_info_list = if promoted.is_empty() {
                    None
                } else {
                    Some(
                        FileInfoList::deserialize(serde_json::Value::Object(promoted))
                            .map_err(de::Error::custom)?,
                    )
                };

                Ok(FileInfoSearchResults {
                    file_info_list,
                    matches,
                })
            }
        }

        deserializer.deserialize_map(ResultsVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_info::FileInfo;
    use crate::file_info_list::FileInfoMap;
    use crate::utils::go_json_marshal;

    fn file_info(id: &str) -> FileInfo {
        FileInfo {
            id: id.into(),
            // Go's `CreatorId` is tagged `json:"user_id"`.
            creator_id: "u1".into(),
            channel_id: "c1".into(),
            create_at: 100,
            update_at: 100,
            name: "one.txt".into(),
            extension: "txt".into(),
            size: 11,
            mime_type: "text/plain".into(),
            ..Default::default()
        }
    }

    fn results() -> FileInfoSearchResults {
        FileInfoSearchResults::new(
            Some(FileInfoList {
                order: Some(vec!["f1".into()]),
                file_infos: Some(FileInfoMap::from([("f1".into(), file_info("f1"))])),
                ..Default::default()
            }),
            Some(FileInfoSearchMatches::from([(
                "f1".into(),
                Some(vec!["hit".into()]),
            )])),
        )
    }

    #[test]
    fn a_nil_embed_drops_five_keys_rather_than_emitting_them() {
        let empty = FileInfoSearchResults::default();
        assert_eq!(go_json_marshal(&empty).unwrap(), r#"{"matches":null}"#);

        // ...and an embed full of zero values emits all five, as nulls and empty scalars.
        let zero = FileInfoSearchResults::new(Some(FileInfoList::default()), None);
        assert_eq!(
            go_json_marshal(&zero).unwrap(),
            r#"{"order":null,"file_infos":null,"next_file_info_id":"","prev_file_info_id":"","first_inaccessible_file_time":0,"matches":null}"#
        );
    }

    #[test]
    fn the_promoted_keys_come_first() {
        let whole = go_json_marshal(&results()).unwrap();
        assert!(
            whole.starts_with(r#"{"order":["f1"],"file_infos":{"#),
            "{whole}"
        );
        assert!(whole.ends_with(r#","matches":{"f1":["hit"]}}"#), "{whole}");
    }

    /// The reason the `Deserialize` is hand-written: a `flatten`ed `Option` would be `Some` here.
    #[test]
    fn only_a_recognised_key_allocates_the_embed() {
        for doc in [
            r#"{}"#,
            r#"{"matches":{}}"#,
            r#"{"matches":null}"#,
            r#"{"nope":1}"#,
        ] {
            let got: FileInfoSearchResults = serde_json::from_str(doc).unwrap();
            assert!(got.file_info_list.is_none(), "{doc}");
        }

        for doc in [
            r#"{"order":null}"#,
            r#"{"next_file_info_id":"n1"}"#,
            r#"{"first_inaccessible_file_time":0}"#,
        ] {
            let got: FileInfoSearchResults = serde_json::from_str(doc).unwrap();
            assert!(got.file_info_list.is_some(), "{doc}");
        }
    }

    /// `matches` distinguishes three states and all three reach the client.
    #[test]
    fn matches_distinguishes_absent_from_empty_from_a_nil_value() {
        let absent: FileInfoSearchResults = serde_json::from_str("{}").unwrap();
        assert!(absent.matches.is_none());

        let empty: FileInfoSearchResults = serde_json::from_str(r#"{"matches":{}}"#).unwrap();
        assert_eq!(empty.matches, Some(FileInfoSearchMatches::new()));

        let nil_value: FileInfoSearchResults =
            serde_json::from_str(r#"{"matches":{"f1":null}}"#).unwrap();
        assert_eq!(
            nil_value.matches,
            Some(FileInfoSearchMatches::from([("f1".into(), None)]))
        );
        assert_eq!(
            go_json_marshal(&nil_value).unwrap(),
            r#"{"matches":{"f1":null}}"#
        );
    }

    #[test]
    fn a_duplicate_matches_key_takes_the_last_value() {
        let got: FileInfoSearchResults =
            serde_json::from_str(r#"{"matches":{"a":["1"]},"matches":{"b":["2"]}}"#).unwrap();
        assert_eq!(
            got.matches,
            Some(FileInfoSearchMatches::from([(
                "b".into(),
                Some(vec!["2".into()])
            )]))
        );
    }

    #[test]
    fn new_assigns_without_materialising_anything() {
        let both_nil = FileInfoSearchResults::new(None, None);
        assert!(both_nil.file_info_list.is_none() && both_nil.matches.is_none());
        assert_eq!(both_nil, FileInfoSearchResults::default());
    }
}

/// Serialization parity against `fixtures/file_info_search_results.json` — the
/// reflection-populated oracle, every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/file_info_search_results.json");
        let decoded: FileInfoSearchResults = serde_json::from_str(raw).unwrap();

        // The fixture carries every promoted key, so the embed must have been allocated.
        assert!(decoded.file_info_list.is_some());
        assert!(decoded.matches.as_ref().is_some_and(|m| !m.is_empty()));

        let ours: serde_json::Value = serde_json::to_value(&decoded).unwrap();
        let theirs: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(ours, theirs);
    }
}

/// Parity tests driven by `fixtures/behaviour_file_info_search_results.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_file_info_search_results.json"
        ))
        .unwrap()
    }

    /// The one document Go decodes and we reject: a nil `*FileInfo` inside `file_infos`. See
    /// [D-033].
    const NIL_INFO_IN_MAP: &str = "nil_info_in_map";

    /// Go matches struct field names case-insensitively and serde does not, so `{"ORDER":[]}`
    /// allocates the embed there and is an unknown key here. See [D-040].
    const UPPERCASE_KEY: &str = "uppercase_key_only";

    #[test]
    fn the_promoted_key_set_matches_go() {
        let oracle = oracle();
        let keys: Vec<&str> = oracle["file_info_list_wire_keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();

        // Order matters as well as membership: it is the emission order of the flattened half.
        assert_eq!(keys, FileInfoList::WIRE_KEYS);
    }

    /// The wire format, byte-for-byte, plus the two facts the JSON cannot show — whether the
    /// embed is nil, and whether `matches` is nil rather than empty.
    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["wire"].as_array().unwrap();
        assert_eq!(cases.len(), 17, "the wire corpus changed size");

        let mut nil_embeds = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let doc = case["in"].as_str().unwrap();
            let out = &case["out"];

            if name == NIL_INFO_IN_MAP {
                // Go stores the nil element and re-emits it; we fail the whole document.
                assert!(
                    serde_json::from_str::<FileInfoSearchResults>(doc).is_err(),
                    "{name}: expected the documented decode failure"
                );
                assert!(out["json"].as_str().unwrap().contains(r#""f1":null"#));
                continue;
            }

            let decoded: FileInfoSearchResults =
                serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}"));

            if name == UPPERCASE_KEY {
                assert!(!out["list_nil"].as_bool().unwrap(), "{name}: Go allocated");
                assert!(
                    decoded.file_info_list.is_none(),
                    "{name}: expected the documented divergence"
                );
                continue;
            }

            assert_eq!(
                go_json_marshal(&decoded).unwrap(),
                out["json"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                decoded.file_info_list.is_none(),
                out["list_nil"].as_bool().unwrap(),
                "{name}: embed nil"
            );
            assert_eq!(
                decoded.matches.is_none(),
                out["matches_nil"].as_bool().unwrap(),
                "{name}: matches nil"
            );

            if out["list_nil"].as_bool().unwrap() {
                nil_embeds += 1;
            }
        }

        // The corpus is only worth anything if it still contains documents that leave the embed
        // nil — that is the case the hand-written `Deserialize` exists for.
        assert_eq!(nil_embeds, 8, "the number of nil-embed documents changed");
    }

    #[test]
    fn make_matches_go() {
        let oracle = oracle();
        let cases = oracle["make"].as_array().unwrap();
        assert_eq!(cases.len(), 8, "the constructor corpus changed size");

        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");
            let out = &case["out"];

            // Rebuilt from Go's own output rather than from a Rust literal, so the construction
            // and the marshalling are checked against the same recording.
            let list = if out["list_nil"].as_bool().unwrap() {
                None
            } else {
                Some(
                    serde_json::from_str::<FileInfoList>(out["list"]["json"].as_str().unwrap())
                        .unwrap_or_else(|e| panic!("{name}: {e}")),
                )
            };
            let matches =
                serde_json::from_str::<FileInfoSearchResults>(out["json"].as_str().unwrap())
                    .unwrap_or_else(|e| panic!("{name}: {e}"))
                    .matches;

            let built = FileInfoSearchResults::new(list, matches);
            assert_eq!(
                go_json_marshal(&built).unwrap(),
                out["json"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                built.file_info_list.is_none(),
                out["list_nil"].as_bool().unwrap(),
                "{name}: embed nil"
            );
            assert_eq!(
                built.matches.is_none(),
                out["matches_nil"].as_bool().unwrap(),
                "{name}: matches nil"
            );
        }
    }

    /// The alias on its own, away from the embed: Go sorts map keys, keeps a nil `[]string` as
    /// `null`, and HTML-escapes — which is why this goes through [`go_json_marshal`] and not
    /// `serde_json::to_string` ([D-027]).
    #[test]
    fn the_matches_map_matches_go() {
        let oracle = oracle();
        let cases = oracle["matches_wire"].as_array().unwrap();
        assert_eq!(cases.len(), 9, "the matches corpus changed size");

        let mut escaped_seen = false;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");

            let want = case["out"].as_str().unwrap();
            let ours: Option<FileInfoSearchMatches> = serde_json::from_str(want).unwrap();
            assert_eq!(
                ours.is_none(),
                case["nil"].as_bool().unwrap(),
                "{name}: nil"
            );
            assert_eq!(go_json_marshal(&ours).unwrap(), want, "{name}");

            if name == "escaped" {
                escaped_seen = true;
                // Plain serde would emit the raw characters, which is the whole of [D-027].
                assert_ne!(serde_json::to_string(&ours).unwrap(), want);
            }
        }
        assert!(escaped_seen);
    }
}
