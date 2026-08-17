//! Port of `model/post_search_results.go` (post_search_results.go:1–56).
//!
//! Fifty-six lines, three of whose four methods are one-line wrappers over [`PostList`]. All of
//! the difficulty is in the type declaration:
//!
//! ```go
//! type PostSearchResults struct {
//!     *PostList
//!     Matches PostSearchMatches `json:"matches"`
//! }
//! ```
//!
//! # The embed flattens, and a nil embed drops six keys
//!
//! `order`, `posts` and the four scalars sit beside `matches` in one flat object — there is no
//! `post_list` key and no nesting. When the pointer is **nil**, `encoding/json` skips every field
//! whose index path runs through it, so the whole document is `{"matches":null}` rather than six
//! nulls. That state is not hypothetical: `MakePostSearchResults` takes the list from its caller
//! and `Auditable` guards for nil explicitly.
//!
//! # Which keys allocate the embed is the wire format
//!
//! Go allocates the embedded pointer lazily, the first time a decode walks into it. Measured:
//!
//! | document | embed after decode |
//! |---|---|
//! | `{}`, `{"matches":{}}`, `{"nope":1}`, `{"burn_on_read_posts":{}}` | nil |
//! | `{"order":null}`, `{"next_post_id":"n1"}`, `{"has_next":false}` | allocated |
//!
//! So the round trip of `{"matches":{}}` is `{"matches":{}}`, while the round trip of
//! `{"order":null}` gains five keys it did not have. That is why [`PostSearchResults`] carries a
//! hand-written [`Deserialize`] keyed on [`PostList::WIRE_KEYS`]: serde's `flatten` on an
//! `Option` always produces `Some`, which would put those five keys on every response.
//!
//! # `ToJSON` mutates its receiver, where [`PostList::to_json`] does not
//!
//! Both open with `x := *o`. `PostList`'s copies the struct that *owns* the map, so the strip
//! swaps the map on the copy; this one copies a struct holding a **pointer**, so the strip lands
//! on the shared list and the caller's integrations are gone. Two lines that read identically,
//! opposite side effects — hence `&mut self` here and `&self` there.
//!
//! # The three methods Go crashes on
//!
//! `ToJSON`, `EncodeJSON` and `ForPlugin` all dereference the embed without checking it, so each
//! panics on exactly the documents the table above leaves nil — including the ordinary
//! `{"matches":{…}}`. Ours answer instead; see [D-054].

use std::collections::BTreeMap;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::post_list::{EncodeJsonError, PostList};
use crate::utils::{StringArray, go_json_marshal};

/// Port of `model.PostSearchMatches` (post_search_results.go:11) — Go's `map[string][]string`.
///
/// A `BTreeMap` because Go sorts map keys byte-wise when marshalling and `String: Ord` is
/// byte-wise too ([D-027]). The value is an `Option` because Go's `[]string` is nillable and the
/// difference reaches the client: `{"p1":null}` round-trips as `null`, `{"p1":[]}` as `[]`.
pub type PostSearchMatches = BTreeMap<String, Option<StringArray>>;

/// Port of `model.PostSearchResults` (post_search_results.go:13).
///
/// `post_list` is an `Option` because Go's embed is a pointer that is nil for every document
/// carrying none of [`PostList::WIRE_KEYS`] — see the module docs for what that costs on the
/// wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PostSearchResults {
    /// The embedded `*PostList`. Flattened, so its keys sit at the top level and precede
    /// `matches` — which is the order Go emits them in, promoted fields first.
    #[serde(flatten)]
    pub post_list: Option<PostList>,

    /// No `omitempty`, so a nil map reaches the client as `null` rather than `{}`.
    #[serde(rename = "matches")]
    pub matches: Option<PostSearchMatches>,
}

impl PostSearchResults {
    /// Port of `model.MakePostSearchResults` (post_search_results.go:18).
    ///
    /// A plain field assignment in Go, including the nil list — which is the state that makes
    /// [`Self::to_json`], [`Self::encode_json`] and [`Self::for_plugin`] crash the Go server.
    pub fn new(post_list: Option<PostList>, matches: Option<PostSearchMatches>) -> Self {
        Self { post_list, matches }
    }

    /// Port of `(*PostSearchResults).ToJSON` (post_search_results.go:25).
    ///
    /// **Takes `&mut self`, unlike [`PostList::to_json`].** Go's `psCopy := *o` copies a struct
    /// holding a pointer, so `StripActionIntegrations` runs against the list the caller still
    /// holds. Reproducing the return value without reproducing the side effect would leave a
    /// caller's integrations in place where Go removed them.
    ///
    /// Go panics when the embed is nil; we marshal what is there ([D-054]).
    pub fn to_json(&mut self) -> Result<String, serde_json::Error> {
        if let Some(list) = self.post_list.as_mut() {
            list.strip_action_integrations();
        }
        go_json_marshal(self)
    }

    /// Port of `(*PostSearchResults).EncodeJSON` (post_search_results.go:32).
    ///
    /// Same side effect as [`Self::to_json`] — Go writes this one out explicitly rather than
    /// through a copy — plus the newline `json.Encoder.Encode` appends and `json.Marshal` does
    /// not.
    pub fn encode_json<W: std::io::Write>(&mut self, w: &mut W) -> Result<(), EncodeJsonError> {
        if let Some(list) = self.post_list.as_mut() {
            list.strip_action_integrations();
        }
        let mut encoded = go_json_marshal(self)?;
        encoded.push('\n');
        w.write_all(encoded.as_bytes())?;
        Ok(())
    }

    /// Port of `(*PostSearchResults).ForPlugin` (post_search_results.go:37).
    ///
    /// Replaces the embed with [`PostList::for_plugin`]'s result, so — unlike [`Self::to_json`]
    /// — the receiver is left alone. One divergence: Go's `plCopy := *o` copies the `Matches`
    /// map header, so the returned value **shares** the map and a write through either is
    /// visible in the other. Measured (`matches_aliased: true`), not inferred. Ours clones, in
    /// the same spirit as [D-024].
    ///
    /// Go panics when the embed is nil; we return a copy that keeps it `None` ([D-054]).
    pub fn for_plugin(&self) -> Self {
        Self {
            post_list: self.post_list.as_ref().map(PostList::for_plugin),
            matches: self.matches.clone(),
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
impl<'de> Deserialize<'de> for PostSearchResults {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ResultsVisitor;

        impl<'de> Visitor<'de> for ResultsVisitor {
            type Value = PostSearchResults;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a post search results object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<PostSearchResults, A::Error>
            where
                A: MapAccess<'de>,
            {
                // Buffered rather than decoded field by field so `PostList`'s own derive stays
                // the single definition of its wire format.
                let mut promoted = serde_json::Map::new();
                let mut matches = None;

                while let Some(key) = map.next_key::<String>()? {
                    if key == "matches" {
                        // A duplicate key takes the last value, as Go does.
                        matches = map.next_value::<Option<PostSearchMatches>>()?;
                    } else if PostList::WIRE_KEYS.contains(&key.as_str()) {
                        map.next_value::<serde_json::Value>()
                            .map(|value| promoted.insert(key, value))?;
                    } else {
                        map.next_value::<de::IgnoredAny>()?;
                    }
                }

                // Empty means no *recognised* key was present, which is exactly when Go leaves
                // the embedded pointer nil. An unknown key does not allocate it; nor does
                // `burn_on_read_posts`, which is `json:"-"` in Go.
                let post_list = if promoted.is_empty() {
                    None
                } else {
                    Some(
                        PostList::deserialize(serde_json::Value::Object(promoted))
                            .map_err(de::Error::custom)?,
                    )
                };

                Ok(PostSearchResults { post_list, matches })
            }
        }

        deserializer.deserialize_map(ResultsVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post::Post;
    use crate::post_list::PostMap;

    fn post(id: &str) -> Post {
        Post {
            id: id.into(),
            create_at: 100,
            update_at: 100,
            channel_id: "c1".into(),
            user_id: "u1".into(),
            ..Default::default()
        }
    }

    fn results() -> PostSearchResults {
        PostSearchResults::new(
            Some(PostList {
                order: Some(vec!["p1".into()]),
                posts: Some(PostMap::from([("p1".into(), post("p1"))])),
                ..Default::default()
            }),
            Some(PostSearchMatches::from([(
                "p1".into(),
                Some(vec!["hit".into()]),
            )])),
        )
    }

    #[test]
    fn a_nil_embed_drops_six_keys_rather_than_emitting_them() {
        let empty = PostSearchResults::default();
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"matches":null}"#
        );
        // ...and an embed full of zero values is a different document entirely.
        let zeroed = PostSearchResults::new(Some(PostList::default()), None);
        assert_eq!(
            serde_json::to_string(&zeroed).unwrap(),
            r#"{"order":null,"posts":null,"next_post_id":"","prev_post_id":"","first_inaccessible_post_time":0,"matches":null}"#
        );
    }

    #[test]
    fn the_embed_is_flat_and_precedes_matches() {
        let json = serde_json::to_string(&results()).unwrap();
        assert!(!json.contains("post_list"), "{json}");
        assert!(json.starts_with(r#"{"order":["p1"],"posts":{"#), "{json}");
        assert!(json.ends_with(r#""matches":{"p1":["hit"]}}"#), "{json}");
    }

    #[test]
    fn only_a_recognised_key_allocates_the_embed() {
        for doc in [
            r#"{}"#,
            r#"{"matches":{}}"#,
            r#"{"nope":1}"#,
            r#"{"burn_on_read_posts":{}}"#,
        ] {
            let decoded: PostSearchResults = serde_json::from_str(doc).unwrap();
            assert!(decoded.post_list.is_none(), "{doc}");
        }
        for doc in [r#"{"order":null}"#, r#"{"has_next":false}"#] {
            let decoded: PostSearchResults = serde_json::from_str(doc).unwrap();
            assert!(decoded.post_list.is_some(), "{doc}");
        }
    }

    #[test]
    fn a_nil_matches_value_survives_the_round_trip() {
        let decoded: PostSearchResults =
            serde_json::from_str(r#"{"matches":{"p1":null,"p2":[]}}"#).unwrap();
        let matches = decoded.matches.as_ref().unwrap();
        assert_eq!(matches["p1"], None);
        assert_eq!(matches["p2"], Some(Vec::new()));
        assert_eq!(
            serde_json::to_string(&decoded).unwrap(),
            r#"{"matches":{"p1":null,"p2":[]}}"#
        );
    }

    #[test]
    fn to_json_strips_the_receiver_and_for_plugin_does_not() {
        let mut with_action: PostSearchResults = serde_json::from_str(
            r#"{"order":["pa"],"posts":{"pa":{"id":"pa","channel_id":"c1","user_id":"u1",
               "props":{"attachments":[{"id":1,"actions":[{"id":"a1","name":"Click",
               "integration":{"url":"https://example.com/hook"}}]}]}}}}"#,
        )
        .unwrap();

        // ForPlugin leaves the receiver alone...
        let _ = with_action.for_plugin();
        assert!(
            serde_json::to_string(&with_action)
                .unwrap()
                .contains("integration")
        );

        // ...ToJSON does not, which is the opposite of PostList::to_json.
        let _ = with_action.to_json().unwrap();
        assert!(
            !serde_json::to_string(&with_action)
                .unwrap()
                .contains("integration")
        );
    }

    #[test]
    fn the_three_nil_embed_methods_answer_where_go_crashes() {
        // [D-054]: every one of these is a panic in Go.
        let mut empty = PostSearchResults::new(None, Some(PostSearchMatches::new()));
        assert_eq!(empty.to_json().unwrap(), r#"{"matches":{}}"#);

        let mut buf = Vec::new();
        empty.encode_json(&mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "{\"matches\":{}}\n");

        assert!(empty.for_plugin().post_list.is_none());
    }
}

/// Serialization parity against `fixtures/post_search_results.json` — the reflection-populated
/// oracle, every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/post_search_results.json");
        let decoded: PostSearchResults = serde_json::from_str(raw).unwrap();

        // The fixture carries every promoted key, so the embed must have been allocated.
        assert!(decoded.post_list.is_some());
        assert!(decoded.matches.as_ref().is_some_and(|m| !m.is_empty()));

        let ours: serde_json::Value = serde_json::to_value(&decoded).unwrap();
        let theirs: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(ours, theirs);
    }
}

/// Parity tests driven by `fixtures/behaviour_post_search_results.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_post_search_results.json"
        ))
        .unwrap()
    }

    fn section<'a>(oracle: &'a Value, key: &str) -> &'a [Value] {
        let cases = oracle[key].as_array().unwrap_or_else(|| panic!("{key}"));
        assert!(!cases.is_empty(), "{key} is empty");
        cases
    }

    fn name(case: &Value) -> &str {
        case["name"].as_str().unwrap()
    }

    fn panicked(case: &Value) -> bool {
        case["panicked"].as_bool().unwrap()
    }

    /// The one document in the corpus our decoder rejects: `posts` holds an explicit `null`,
    /// which Go's `map[string]*Post` accepts. Not reachable here — no corpus case has one — but
    /// the helper documents why a decode is `unwrap`ped. See [D-033].
    fn decode(doc: &str) -> PostSearchResults {
        serde_json::from_str(doc).unwrap_or_else(|e| panic!("{doc}: {e}"))
    }

    /// Byte-for-byte unless the answer carries a **rewritten** attachment list, where Go's struct
    /// field order and our sorted `serde_json::Map` disagree — see [D-048]. Same carve-out as
    /// `post_list::go_parity`.
    fn assert_json_matches_go(what: &str, ours: &str, want: &str) {
        if want.contains(r#""attachments":[{"id":"#) {
            let ours: Value = serde_json::from_str(ours).unwrap();
            let want: Value = serde_json::from_str(want).unwrap();
            assert_eq!(ours, want, "{what}");
        } else {
            assert_eq!(ours, want, "{what}");
        }
    }

    /// Asserts a whole value against a `dumpSearchResults` record: the exact bytes plus the
    /// nil-ness of the embed and of `matches`, neither of which the JSON alone can show — a nil
    /// embed and an absent one look the same from outside.
    fn assert_dump(actual: &PostSearchResults, expected: &Value, what: &str) {
        assert_json_matches_go(
            &format!("{what}: bytes"),
            &go_json_marshal(actual).unwrap(),
            expected["json"].as_str().unwrap(),
        );
        assert_eq!(
            actual.post_list.is_none(),
            expected["list_nil"].as_bool().unwrap(),
            "{what}: embed nil-ness"
        );
        assert_eq!(
            actual.matches.is_none(),
            expected["matches_nil"].as_bool().unwrap(),
            "{what}: matches nil-ness"
        );
    }

    /// The Go documents whose promoted key is one only Go recognises — it matches field names
    /// case-insensitively and we do not. [D-040], asserted so it cannot rot into a surprise.
    const CASE_INSENSITIVE_ONLY: &str = "uppercase_key_only";

    #[test]
    fn the_promoted_key_set_matches_go() {
        // Read off the Go struct tags by the oracle, so a field added upstream fails here rather
        // than silently changing which documents allocate the embed.
        let keys: Vec<String> = serde_json::from_value(oracle()["post_list_wire_keys"].clone())
            .expect("post_list_wire_keys");
        assert_eq!(keys, PostList::WIRE_KEYS);
    }

    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "wire") {
            let what = name(case);
            assert!(!panicked(case), "{what}: Go panicked decoding");
            if what == CASE_INSENSITIVE_ONLY {
                continue;
            }
            assert_dump(&decode(case["in"].as_str().unwrap()), &case["out"], what);
        }
    }

    /// [D-040] in one case: Go's decoder is case-insensitive, so `{"ORDER":[]}` allocates the
    /// embed and fills `order`; ours treats it as an unknown key and leaves the embed nil.
    #[test]
    fn an_uppercase_key_allocates_the_embed_in_go_and_not_here() {
        let oracle = oracle();
        let case = section(&oracle, "wire")
            .iter()
            .find(|c| name(c) == CASE_INSENSITIVE_ONLY)
            .expect(CASE_INSENSITIVE_ONLY);

        assert!(!case["out"]["list_nil"].as_bool().unwrap(), "Go allocated");
        assert_eq!(
            case["out"]["json"].as_str().unwrap(),
            r#"{"order":[],"posts":null,"next_post_id":"","prev_post_id":"","first_inaccessible_post_time":0,"matches":null}"#
        );

        let ours = decode(case["in"].as_str().unwrap());
        assert!(ours.post_list.is_none(), "ours did not");
        assert_eq!(go_json_marshal(&ours).unwrap(), r#"{"matches":null}"#);
    }

    #[test]
    fn make_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "make") {
            let what = name(case);
            assert!(!panicked(case), "{what}");
            let built = match what {
                "both_nil" | "zero_value" => PostSearchResults::new(None, None),
                "nil_list_with_matches" => PostSearchResults::new(
                    None,
                    Some(PostSearchMatches::from([(
                        "p1".into(),
                        Some(vec!["alpha".into()]),
                    )])),
                ),
                "new_list_nil_matches" => PostSearchResults::new(Some(PostList::new()), None),
                "zero_list_nil_matches" => PostSearchResults::new(Some(PostList::default()), None),
                "new_list_empty_matches" => {
                    PostSearchResults::new(Some(PostList::new()), Some(PostSearchMatches::new()))
                }
                "decoded_list" => PostSearchResults::new(
                    Some(
                        serde_json::from_str(
                            r#"{"order":["p1"],"posts":{"p1":{"id":"p1","create_at":100,
                               "update_at":100,"user_id":"u1","channel_id":"c1","message":"one"}}}"#,
                        )
                        .unwrap(),
                    ),
                    Some(PostSearchMatches::from([(
                        "p1".into(),
                        Some(vec!["one".into()]),
                    )])),
                ),
                other => panic!("unhandled make case {other}"),
            };
            assert_dump(&built, &case["out"], what);
        }
    }

    #[test]
    fn to_json_matches_go_and_strips_the_receiver() {
        let oracle = oracle();
        let mut go_panics = 0;
        for case in section(&oracle, "to_json") {
            let what = name(case);
            if what == CASE_INSENSITIVE_ONLY {
                continue;
            }
            let mut ours = decode(case["in"].as_str().unwrap());

            if panicked(case) {
                // [D-054]: Go dereferences the nil embed. Assert that it is exactly the nil-embed
                // documents that crash, and that ours answers with what Go would have emitted.
                go_panics += 1;
                assert!(ours.post_list.is_none(), "{what}: not a nil embed");
                let answered = ours.to_json().unwrap();
                assert!(answered.starts_with(r#"{"matches""#), "{what}: {answered}");
                continue;
            }

            assert!(!case["err"].as_bool().unwrap(), "{what}: Go errored");
            assert_json_matches_go(
                &format!("{what}: to_json"),
                &ours.to_json().unwrap(),
                case["out"].as_str().unwrap(),
            );
            // The side effect is the point: Go's receiver is stripped too.
            assert_dump(&ours, &case["receiver_after"], &format!("{what}: receiver"));
        }
        assert_eq!(go_panics, 9, "the nil-embed documents Go crashes on");
    }

    #[test]
    fn encode_json_matches_go_including_the_trailing_newline() {
        let oracle = oracle();
        for case in section(&oracle, "encode_json") {
            let what = name(case);
            if what == CASE_INSENSITIVE_ONLY {
                continue;
            }
            let mut ours = decode(case["in"].as_str().unwrap());

            if panicked(case) {
                assert!(ours.post_list.is_none(), "{what}: not a nil embed");
                continue;
            }

            let mut buf = Vec::new();
            ours.encode_json(&mut buf).unwrap();
            let written = String::from_utf8(buf).unwrap();
            assert!(written.ends_with('\n'), "{what}: no trailing newline");
            assert_json_matches_go(
                &format!("{what}: encode_json"),
                &written,
                case["out"].as_str().unwrap(),
            );
            assert_dump(&ours, &case["receiver_after"], &format!("{what}: receiver"));
        }
    }

    #[test]
    fn for_plugin_matches_go_and_leaves_the_receiver_alone() {
        let oracle = oracle();
        let mut aliased = 0;
        for case in section(&oracle, "for_plugin") {
            let what = name(case);
            if what == CASE_INSENSITIVE_ONLY {
                continue;
            }
            let ours = decode(case["in"].as_str().unwrap());

            if panicked(case) {
                // [D-054] again: Clone dereferences the nil embed.
                assert!(ours.post_list.is_none(), "{what}: not a nil embed");
                assert!(ours.for_plugin().post_list.is_none(), "{what}");
                continue;
            }

            assert_dump(&ours.for_plugin(), &case["out"], what);
            assert_dump(&ours, &case["original_after"], &format!("{what}: original"));

            // Go shares the Matches map with the copy; ours clones it. Measured, not assumed.
            if case["matches_aliased"].as_bool() == Some(true) {
                aliased += 1;
                let mut copy = ours.for_plugin();
                copy.matches
                    .as_mut()
                    .unwrap()
                    .insert("injected".into(), Some(vec!["x".into()]));
                assert!(
                    !ours.matches.as_ref().unwrap().contains_key("injected"),
                    "{what}: ours aliased the map"
                );
            }
        }
        assert_eq!(aliased, 5, "the corpus cases where Go shares the map");
    }

    #[test]
    fn the_matches_map_matches_go() {
        let oracle = oracle();
        for case in section(&oracle, "matches_wire") {
            let what = name(case);
            assert!(!panicked(case), "{what}");
            assert!(case["err"].is_null(), "{what}: Go failed to decode");

            let ours: Option<PostSearchMatches> =
                serde_json::from_str(case["in"].as_str().unwrap()).unwrap();
            assert_eq!(
                ours.is_none(),
                case["nil"].as_bool().unwrap(),
                "{what}: nil-ness"
            );
            assert_eq!(
                ours.as_ref().map_or(0, PostSearchMatches::len) as u64,
                case["len"].as_u64().unwrap(),
                "{what}: len"
            );
            // Byte-for-byte, which is what pins the key sort and Go's HTML escaping.
            assert_eq!(
                go_json_marshal(&ours).unwrap(),
                case["out"].as_str().unwrap(),
                "{what}"
            );
        }
    }
}
