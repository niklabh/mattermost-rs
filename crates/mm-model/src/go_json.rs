//! Go's `encoding/json` field matching — the half serde does not have.
//!
//! serde matches a `rename` string byte for byte and silently ignores anything else. Go tries the
//! exact name first and then falls back to a **case-insensitive** match (decode.go:699), so
//! `{"Title":"t"}` and `{"TITLE":"t"}` and `{"tItLe":"t"}` all populate a field tagged `title`.
//! [D-040] measured the divergence through `Post::attachments`, which is where client-supplied
//! JSON — a webhook payload, a slash-command response — becomes a model type: Go reads it, we
//! dropped it, and the two servers then disagreed about a post neither had rejected.
//!
//! # The approach: rewrite the keys, do not rewrite the deserializer
//!
//! [`remap_object_keys`] takes a `serde_json::Value` and a [`GoFields`] schema and renames every
//! key Go would have folded onto a field into that field's exact name. The derived `Deserialize`
//! then does the rest unchanged. The alternative — a case-insensitive `Visitor` on every type in
//! the crate — was rejected in [D-040]'s own options list as untractable, and it would have to be
//! repeated for all 71 modules; this is one function that a type opts into by declaring its field
//! names.
//!
//! # Why an ASCII-only fold table is exact rather than approximate
//!
//! Go's `foldName` upper-cases ASCII and pushes every other rune through `foldRune`, which is the
//! smallest rune of its `unicode.SimpleFold` orbit. Reproducing that in general means carrying
//! Unicode's whole fold table. It is not needed: **every `json:` name in the Mattermost tree is
//! ASCII**, so a non-ASCII key can only ever match one if its own fold is ASCII — and a sweep of
//! the entire scalar range (recorded in `fixtures/behaviour_json_fold.json`) finds exactly **two**
//! runes that qualify: U+017F LATIN SMALL LETTER LONG S folds to `S`, and U+212A KELVIN SIGN folds
//! to `K`. Both are carried below. Every other non-ASCII rune folds to something non-ASCII, which
//! can never equal a byte of an ASCII field name, so mapping it to itself gives the right *answer*
//! even where it is not the right *fold*.
//!
//! [`assert_ascii_field_names`] is the precondition made checkable, and every schema in the tree
//! is run through it by a test.
//!
//! # What this deliberately does not reproduce
//!
//! Go resolves each key as it reads it and assigns, so with two keys mapping to the same field the
//! **last one in the document** wins. A `serde_json::Map` is a `BTreeMap` — this crate does not
//! enable `preserve_order`, because Go marshals a `map[string]any` with sorted keys and turning
//! that on would change every props object we emit — so document order is gone before this
//! function sees it. It costs nothing in practice: two keys that map to the same field differ only
//! by case, therefore have the **same length**, and Postgres `jsonb` orders equal-length keys
//! bytewise, which is exactly `BTreeMap`'s order. Both servers read these props out of the same
//! `jsonb` column, so both see Postgres's order and not the author's. Pinned by
//! `duplicate_keys_resolve_in_postgres_order`.

use serde_json::Value;

/// The JSON names of one struct, plus the keys under which it nests another.
///
/// Declaration order is load-bearing: Go builds its folded index with "first folded match takes
/// precedence" (encode.go:1306), so when two names fold together the earlier one wins. Keep
/// [`GoFields::names`] in the order the Rust struct declares its fields.
#[derive(Debug, Clone, Copy)]
pub struct GoFields {
    /// Every `#[serde(rename = "…")]` on the struct, in declaration order.
    pub names: &'static [&'static str],
    /// `(key, schema)` for each field that holds an object, or an array of objects, with `json:`
    /// tags of its own. The key is the **exact** name, because this level is remapped first.
    pub nested: &'static [(&'static str, &'static GoFields)],
}

/// The two non-ASCII runes whose `foldRune` result is an ASCII byte.
///
/// Complete: `reference/dump/behaviour_json_fold.go` sweeps `0..=char::MAX` and finds no others.
/// Regenerating the fixture is what would catch a Unicode revision adding a third.
const ASCII_FOLD_SOURCES: [(char, u8); 2] = [('\u{017F}', b'S'), ('\u{212A}', b'K')];

/// Port of `foldName` (encoding/json/fold.go:16), exact for any name that can match an ASCII one.
///
/// ASCII lower-case folds up; the two runes above fold to their ASCII byte; everything else is
/// passed through, which is not Go's fold but is indistinguishable from it for every comparison
/// this crate makes — see the module doc.
pub fn fold_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii() {
            out.push(c.to_ascii_uppercase());
            continue;
        }
        match ASCII_FOLD_SOURCES.iter().find(|(from, _)| *from == c) {
            Some((_, to)) => out.push(char::from(*to)),
            None => out.push(c),
        }
    }
    out
}

/// Rename every key Go would have folded onto a field into that field's exact name, in place.
///
/// A key that already matches a name exactly is left alone — that is Go's first branch, and it
/// also means a schema listing a name it does not have costs nothing. A key that matches nothing
/// is left alone too, so serde ignores it exactly as Go does.
///
/// Recurses into [`GoFields::nested`] **after** this level, so a nested key that itself arrived
/// folded (`"FIELDS"`) has already become `"fields"` and is found.
pub fn remap_object_keys(value: &mut Value, schema: &GoFields) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    // Which exact names the document *originally* carried. Captured before any rename, because
    // the rule below distinguishes "the author wrote the exact key" from "we produced it".
    let exact_present: Vec<&'static str> = schema
        .names
        .iter()
        .filter(|name| object.contains_key(**name))
        .copied()
        .collect();

    // Go's folded index: first declaration wins a collision.
    let mut renames: Vec<(String, &'static str)> = Vec::new();
    for key in object.keys() {
        if schema.names.contains(&key.as_str()) {
            continue;
        }
        let folded = fold_name(key);
        if let Some(name) = schema
            .names
            .iter()
            .find(|name| fold_name(name) == folded)
            .copied()
        {
            renames.push((key.clone(), name));
        }
    }

    // Go resolves keys in document order and assigns, so the **last** key that lands on a field
    // wins. The order this map preserves — sorted — is the order Postgres `jsonb` hands *both*
    // servers: two keys that fold together differ only by case and therefore have equal length,
    // which is where `jsonb`'s (length, bytewise) ordering and `BTreeMap`'s bytewise ordering
    // agree. So `renames` is already in the order Go would have seen, and applying it in sequence
    // reproduces last-write-wins — see the module doc.
    for (from, to) in renames {
        // …with one exception, and it is not an ordering rule: a key the author wrote **exactly**
        // always beats a folded one, because an upper-case spelling sorts before its lower-case
        // twin on both sides, so the exact key is always the later assignment. Checking
        // `exact_present` rather than `object.contains_key` is what keeps that from also
        // swallowing the second of two *folded* keys, which must overwrite the first.
        if exact_present.contains(&to) {
            object.remove(&from);
            continue;
        }
        if let Some(moved) = object.remove(&from) {
            object.insert(to.to_owned(), moved);
        }
    }

    for (key, nested) in schema.nested {
        let Some(child) = object.get_mut(*key) else {
            continue;
        };
        match child {
            Value::Array(elements) => {
                for element in elements {
                    remap_object_keys(element, nested);
                }
            }
            other => remap_object_keys(other, nested),
        }
    }
}

/// The precondition [`fold_name`]'s partial table rests on: every name in the schema is ASCII.
///
/// Returns the first name that is not, so a test can name it. A non-ASCII `json:` tag would make
/// the two-rune table insufficient and the fold silently wrong for that field alone.
pub fn non_ascii_field_name(schema: &GoFields) -> Option<&'static str> {
    schema
        .names
        .iter()
        .find(|name| !name.is_ascii())
        .copied()
        .or_else(|| {
            schema
                .nested
                .iter()
                .find_map(|(_, nested)| non_ascii_field_name(nested))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELD: GoFields = GoFields {
        names: &["title", "value", "short"],
        nested: &[],
    };
    const ATTACHMENT: GoFields = GoFields {
        names: &["id", "title", "title_link", "author_name", "ts", "fields"],
        nested: &[("fields", &FIELD)],
    };

    fn remapped(json: &str) -> Value {
        let mut value: Value = serde_json::from_str(json).expect("valid JSON");
        remap_object_keys(&mut value, &ATTACHMENT);
        value
    }

    #[test]
    fn an_exact_key_is_untouched() {
        assert_eq!(
            remapped(r#"{"title":"t"}"#),
            serde_json::json!({"title":"t"})
        );
    }

    #[test]
    fn every_casing_lands_on_the_field() {
        for spelling in ["Title", "TITLE", "tItLe", "TiTlE"] {
            assert_eq!(
                remapped(&format!(r#"{{"{spelling}":"t"}}"#)),
                serde_json::json!({"title":"t"}),
                "{spelling} should fold onto title"
            );
        }
    }

    /// Underscores are not separators to the fold — they are ordinary bytes — so `authorname`
    /// matches nothing while `AUTHOR_NAME` matches. Go's answer, and the shape a hand-written
    /// webhook payload is most likely to get wrong in the *unrecoverable* direction.
    #[test]
    fn only_case_is_folded_never_punctuation() {
        assert_eq!(
            remapped(r#"{"AUTHOR_NAME":"a"}"#),
            serde_json::json!({"author_name":"a"})
        );
        assert_eq!(
            remapped(r#"{"authorname":"a"}"#),
            serde_json::json!({"authorname":"a"}),
            "no underscore, no match"
        );
        assert_eq!(
            remapped(r#"{"author-name":"a"}"#),
            serde_json::json!({"author-name":"a"})
        );
    }

    #[test]
    fn an_unknown_key_is_left_for_serde_to_ignore() {
        assert_eq!(
            remapped(r#"{"nosuchkey":"x"}"#),
            serde_json::json!({"nosuchkey":"x"})
        );
    }

    /// The two runes from the sweep, each aimed at a field it can actually reach.
    #[test]
    fn the_two_ascii_folding_runes_reach_their_fields() {
        assert_eq!(
            remapped("{\"t\u{017F}\":123}"),
            serde_json::json!({"ts":123})
        );
        assert_eq!(
            remapped("{\"title_lin\u{212A}\":\"l\"}"),
            serde_json::json!({"title_link":"l"})
        );
    }

    /// A rune that folds to something non-ASCII cannot match an ASCII name, and passing it through
    /// unchanged is what makes that true.
    #[test]
    fn a_rune_that_folds_outside_ascii_matches_nothing() {
        assert_eq!(
            remapped("{\"tit\u{017F}e\":\"x\"}"),
            serde_json::json!({"tit\u{017F}e":"x"}),
            "folds to TITSE, which is not TITLE"
        );
        assert_eq!(
            remapped("{\"caf\u{e9}\":\"x\"}"),
            serde_json::json!({"caf\u{e9}":"x"})
        );
    }

    /// Nesting is remapped, and the nested key may itself have arrived folded.
    #[test]
    fn nested_objects_are_remapped_through_a_folded_key() {
        let mut value: Value =
            serde_json::from_str(r#"{"FIELDS":[{"TITLE":"t","Short":true}]}"#).expect("valid");
        remap_object_keys(&mut value, &ATTACHMENT);
        assert_eq!(
            value,
            serde_json::json!({"fields":[{"title":"t","short":true}]})
        );
    }

    /// A nested value that is an object rather than an array is remapped too — Go's decoder does
    /// not care which, and neither does this.
    #[test]
    fn a_nested_object_is_remapped_as_well_as_an_array() {
        let mut value: Value = serde_json::from_str(r#"{"Fields":{"TITLE":"t"}}"#).expect("valid");
        remap_object_keys(&mut value, &ATTACHMENT);
        assert_eq!(value, serde_json::json!({"fields":{"title":"t"}}));
    }

    /// An exact key present alongside a folded one keeps its own value: the folded key is dropped
    /// rather than overwriting it. See the module doc for why the alternative is unreachable.
    #[test]
    fn an_exact_key_is_not_overwritten_by_a_folded_one() {
        assert_eq!(
            remapped(r#"{"title":"exact","TiTlE":"folded"}"#),
            serde_json::json!({"title":"exact"})
        );
    }

    /// Two *folded* keys and no exact one: the later in sorted order wins, because that is the
    /// later assignment in Go. The rule above must not swallow the second one as though the first
    /// had been exact — which is the whole reason the exact set is captured before any rename.
    #[test]
    fn the_later_of_two_folded_keys_wins() {
        assert_eq!(
            remapped(r#"{"TiTlE":"a","tItLe":"b"}"#),
            serde_json::json!({"title":"b"}),
            "`TiTlE` sorts before `tItLe`, so `tItLe` is the later assignment"
        );
    }

    /// Not an object — a scalar, an array, a null — is returned untouched rather than panicking.
    #[test]
    fn a_non_object_is_left_alone() {
        for json in ["null", "3", r#""s""#, "[1,2]"] {
            let mut value: Value = serde_json::from_str(json).expect("valid");
            let before = value.clone();
            remap_object_keys(&mut value, &ATTACHMENT);
            assert_eq!(value, before, "{json}");
        }
    }

    #[test]
    fn the_schema_precondition_holds_for_this_fixture() {
        assert_eq!(non_ascii_field_name(&ATTACHMENT), None);
    }

    #[test]
    fn a_non_ascii_name_is_reported() {
        const BAD: GoFields = GoFields {
            names: &["ok", "caf\u{e9}"],
            nested: &[],
        };
        assert_eq!(non_ascii_field_name(&BAD), Some("caf\u{e9}"));
    }
}

/// Parity against `fixtures/behaviour_json_fold.json`.
#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_json_fold.json"))
            .expect("behaviour_json_fold.json is generated by reference/dump")
    }

    #[test]
    fn fold_name_matches_go() {
        let oracle = oracle();
        let cases = oracle["fold_name"].as_array().expect("an array");
        assert!(cases.len() >= 25);

        for case in cases {
            let input = case["in"].as_str().expect("an input");
            let expected = case["out"].as_str().expect("an output");
            if !expected.is_ascii() {
                // A name whose fold leaves ASCII cannot match any `json:` tag in the tree, so this
                // port deliberately does not reproduce it — see the module doc. Skipped here
                // rather than silently passing: the assertion below is that it *stays* non-ASCII,
                // which is the property the shortcut actually rests on.
                assert!(
                    !fold_name(input).is_ascii(),
                    "{input:?} folds outside ASCII in Go and must not fold into it here"
                );
                continue;
            }
            assert_eq!(fold_name(input), expected, "foldName({input:?})");
        }
    }

    /// The sweep found exactly two runes, and the port carries exactly those two.
    ///
    /// This is the assertion that makes the partial table honest: if a Unicode revision ever adds
    /// a third, regenerating the fixture fails here rather than producing a key that Go matches
    /// and we drop.
    #[test]
    fn the_ascii_fold_table_is_the_whole_of_gos() {
        let oracle = oracle();
        let sources = oracle["ascii_fold_sources"].as_array().expect("an array");

        let from_go: Vec<(u32, u32)> = sources
            .iter()
            .map(|row| {
                (
                    row["rune"].as_u64().expect("a rune") as u32,
                    row["folded"].as_u64().expect("a fold") as u32,
                )
            })
            .collect();
        let ours: Vec<(u32, u32)> = ASCII_FOLD_SOURCES
            .iter()
            .map(|(from, to)| (*from as u32, u32::from(*to)))
            .collect();

        assert_eq!(
            from_go, ours,
            "Go's sweep of every scalar value disagrees with the table this module carries"
        );
    }

    /// End-to-end: every corpus document, remapped and then decoded into the same shape Go
    /// unmarshalled it into.
    ///
    /// The oracle's struct declares both `title` and `TITLE`, which the real `MessageAttachment`
    /// does not — it is there to pin the collision rule. The `TITLE` field is therefore modelled
    /// here as a second name in the schema, in the same declaration order.
    #[test]
    fn decoding_matches_go_for_every_corpus_document() {
        const ATTACHMENT_LIKE: GoFields = GoFields {
            names: &[
                "id",
                "title",
                "TITLE",
                "title_link",
                "author_name",
                "text",
                "ts",
            ],
            nested: &[],
        };

        let oracle = oracle();
        let cases = oracle["decode"].as_array().expect("an array");
        assert!(cases.len() >= 20);

        // Go's answer depends on **key order**, and a `serde_json::Map` has sorted the keys before
        // anything here runs — as has the `jsonb` column both servers read these props out of. So
        // each document is compared against `sorted_out`: Go's answer for the same document with
        // its keys in bytewise order, which the generator records alongside the author's order for
        // exactly this reason. Comparing against `out` would be asserting that this port
        // reproduces an ordering neither server can observe.
        let mut reordered = 0;
        for case in cases {
            let document = case["in"].as_str().expect("a document");
            let expected = &case["sorted_out"];
            if case["sorted_in"].as_str() != Some(document) {
                reordered += 1;
            }

            let mut value: Value = serde_json::from_str(document).expect("the corpus is valid");
            remap_object_keys(&mut value, &ATTACHMENT_LIKE);
            let object = value
                .as_object()
                .expect("every corpus document is an object");

            for name in ATTACHMENT_LIKE.names {
                let want = &expected[*name];
                // Go's zero values: "" for the strings, 0 for id, null for the `any` ts.
                let got = object.get(*name);
                let matches = match got {
                    Some(actual) => actual == want,
                    None => {
                        want == &Value::from("") || want == &Value::from(0) || want == &Value::Null
                    }
                };
                assert!(
                    matches,
                    "{document}: field {name} is {got:?} here and {want} in Go"
                );
            }
        }

        assert!(
            reordered >= 1,
            "no corpus document is re-ordered by sorting, so the claim that this port answers for \
             the *stored* key order rather than the author's is never exercised"
        );
    }
}
