//! Port of `server/public/model/post_metadata.go`.
//!
//! The bag of rendering data hung off `Post.Metadata`. Almost no logic — the substance is the
//! wire format and one badly-behaved `Copy`.
//!
//! **Every field carries `omitempty`**, collections included. Go's `omitempty` drops a nil slice
//! *and* an empty one, so the two are indistinguishable on the wire and a plain `Vec` with a
//! length predicate is the faithful port, not `Option<Vec>`.
//!
//! [`PostPriority`] is declared in `post.go`, not here — but `PostMetadata` embeds it and
//! `Post` embeds `PostMetadata`, so the two Go files are mutually dependent and something has to
//! break the cycle. It lives here; `post.rs` should re-export it when it lands.
//!
//! Pinned by `fixtures/post_metadata.json`, `fixtures/post_image.json`,
//! `fixtures/post_translation.json`, `fixtures/post_priority.json` and
//! `fixtures/behaviour_post_metadata.json`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::emoji::Emoji;
use crate::file_info::FileInfo;
use crate::post_acknowledgement::PostAcknowledgement;
use crate::post_embed::PostEmbed;
use crate::reaction::Reaction;

/// Go's `json.RawMessage` under an `omitempty` tag.
///
/// `RawMessage` is a `[]byte`, so `omitempty` drops it when the byte slice is empty — but a
/// RawMessage holding the four bytes `null` is **not** empty, and marshals as `null`. That is a
/// state serde's `Option` collapses: by default `null` deserialises to `None`, which would then
/// be omitted on the way out, where Go re-emits `null`.
///
/// This deserialiser wraps whatever it is given — `Value::Null` included — in `Some`, leaving
/// `None` to mean only "key absent". Together with `skip_serializing_if` that reproduces Go's
/// three states exactly. Pinned by the `translation_object_null` oracle case.
mod go_raw_message {
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
        Value::deserialize(d).map(Some)
    }
}

/// Port of `model.PostImage` (post_metadata.go:80) — the dimensions of an external image.
///
/// No field carries `omitempty`, so all four keys are always present.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostImage {
    #[serde(rename = "width")]
    pub width: i64,

    #[serde(rename = "height")]
    pub height: i64,

    /// The `image/…` format name as Go's `image` package uses it: `png`, `gif`, `jpeg`.
    #[serde(rename = "format")]
    pub format: String,

    /// Frames in an animated GIF; `0` for every other format.
    #[serde(rename = "frame_count")]
    pub frame_count: i64,
}

/// Port of `model.PostTranslation` (post_metadata.go:50).
///
/// `text` and `object` are alternatives selected by `type`; both carry `omitempty`. `type` and
/// `state` do not, so they are present even when empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostTranslation {
    /// Used when `type` is `"string"`.
    #[serde(rename = "text", skip_serializing_if = "String::is_empty")]
    pub text: String,

    /// Used when `type` is `"object"`. See [`go_raw_message`] for why `null` is not `None`.
    #[serde(
        rename = "object",
        skip_serializing_if = "Option::is_none",
        deserialize_with = "go_raw_message::deserialize"
    )]
    pub object: Option<serde_json::Value>,

    #[serde(rename = "type")]
    pub type_: String,

    #[serde(rename = "state")]
    pub state: String,

    /// The original language of the post.
    #[serde(rename = "source_lang", skip_serializing_if = "String::is_empty")]
    pub source_lang: String,
}

/// Port of `model.PostPriority` (post.go:230), which lives in `post.go` rather than here.
///
/// **`post_id` and `channel_id` are not the wire names.** Go tags them `json:",omitempty"` — an
/// empty name — and falls back to the *Go field name*, so the keys are `PostId` and `ChannelId`,
/// capitalised, sitting beside the snake_case ones. Same trap as `TeamForExport.SchemeName`.
/// They are documented as internal DB plumbing and still reach the wire.
///
/// The three pointer fields have plain tags with no `omitempty`, so they write `null` when unset
/// rather than disappearing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostPriority {
    #[serde(rename = "priority")]
    pub priority: Option<String>,

    #[serde(rename = "requested_ack")]
    pub requested_ack: Option<bool>,

    #[serde(rename = "persistent_notifications")]
    pub persistent_notifications: Option<bool>,

    /// Wire key is `PostId` — see the type docs.
    #[serde(rename = "PostId", skip_serializing_if = "String::is_empty")]
    pub post_id: String,

    /// Wire key is `ChannelId` — see the type docs.
    #[serde(rename = "ChannelId", skip_serializing_if = "String::is_empty")]
    pub channel_id: String,
}

/// Port of `model.PostMetadata` (post_metadata.go:11).
///
/// Field order matches Go's declaration order, which is the order `encoding/json` emits — note
/// `redacted_file_count` sits between `files` and `images`, not at the end.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostMetadata {
    /// OpenGraph and other embedded-content metadata for links in the post.
    #[serde(rename = "embeds", skip_serializing_if = "Vec::is_empty")]
    pub embeds: Vec<PostEmbed>,

    /// Every custom emoji used in the post, its reactions, its attachments and its interactive
    /// payloads.
    #[serde(rename = "emojis", skip_serializing_if = "Vec::is_empty")]
    pub emojis: Vec<Emoji>,

    #[serde(rename = "files", skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileInfo>,

    /// Set when an ABAC policy stripped the attachments, so the client can render a placeholder.
    #[serde(rename = "redacted_file_count", skip_serializing_if = "is_zero_i64")]
    pub redacted_file_count: i64,

    /// Dimensions of every **external** image in the post, keyed by URL. Does not include file
    /// attachments — those dimensions live on [`FileInfo`].
    #[serde(rename = "images", skip_serializing_if = "HashMap::is_empty")]
    pub images: HashMap<String, PostImage>,

    #[serde(rename = "reactions", skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,

    #[serde(rename = "priority", skip_serializing_if = "Option::is_none")]
    pub priority: Option<PostPriority>,

    #[serde(rename = "acknowledgements", skip_serializing_if = "Vec::is_empty")]
    pub acknowledgements: Vec<PostAcknowledgement>,

    /// Translation data per target language, keyed by language code.
    #[serde(rename = "translations", skip_serializing_if = "HashMap::is_empty")]
    pub translations: HashMap<String, PostTranslation>,

    #[serde(rename = "expire_at", skip_serializing_if = "is_zero_i64")]
    pub expire_at: i64,

    #[serde(rename = "recipients", skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<String>,
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

impl PostMetadata {
    /// Port of `(*PostMetadata).Copy` (post_metadata.go:92).
    ///
    /// Go's comment says "does a deep copy". It does not, in two separate ways, and both are
    /// reproduced because a caller may depend on either:
    ///
    /// 1. **It drops `expire_at` and `recipients`.** They are simply absent from the struct
    ///    literal it returns, so a copied metadata loses both. Almost certainly an upstream
    ///    oversight — fields were added to the struct and not to `Copy` — but "fixing" it would
    ///    make the Rust server carry data the Go server discards. See D-034.
    ///
    /// 2. **Only `Priority` is genuinely deep-copied.** The slices and maps are copied with
    ///    `copy`/`maps.Copy`, which duplicates the *pointers*, so the copy and the original
    ///    share every embed, emoji, file, reaction, acknowledgement, image and translation.
    ///    Rust owns its values, so ours are independent — a divergence in the safe direction,
    ///    and the same class as [D-015] on `Channel::deep_copy`.
    ///
    /// Go also turns nil collections into empty non-nil ones. That is invisible on the wire,
    /// since `omitempty` drops both.
    #[must_use]
    pub fn copy(&self) -> Self {
        Self {
            embeds: self.embeds.clone(),
            emojis: self.emojis.clone(),
            files: self.files.clone(),
            redacted_file_count: self.redacted_file_count,
            images: self.images.clone(),
            reactions: self.reactions.clone(),
            priority: self.priority.clone(),
            acknowledgements: self.acknowledgements.clone(),
            translations: self.translations.clone(),
            // expire_at and recipients are deliberately NOT carried: Go drops them. Left at
            // their defaults rather than omitted from the literal, so the omission is visible.
            expire_at: 0,
            recipients: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn round_trips_the_generated_fixtures() {
        macro_rules! round_trip {
            ($ty:ty, $path:literal) => {{
                let raw = include_str!($path);
                let parsed: $ty = serde_json::from_str(raw).unwrap();
                let original: Value = serde_json::from_str(raw).unwrap();
                assert_eq!(serde_json::to_value(&parsed).unwrap(), original, $path);
            }};
        }
        round_trip!(PostMetadata, "../../../fixtures/post_metadata.json");
        round_trip!(PostImage, "../../../fixtures/post_image.json");
        round_trip!(PostTranslation, "../../../fixtures/post_translation.json");
        round_trip!(PostPriority, "../../../fixtures/post_priority.json");
    }

    #[test]
    fn nil_and_empty_collections_are_both_omitted() {
        // Go cannot tell them apart under omitempty, so neither can we — and that is correct.
        let zero = crate::utils::go_json_marshal(&PostMetadata::default()).unwrap();
        assert_eq!(zero, "{}");

        let explicit_empty = PostMetadata {
            embeds: Vec::new(),
            images: HashMap::new(),
            recipients: Vec::new(),
            ..Default::default()
        };
        assert_eq!(
            crate::utils::go_json_marshal(&explicit_empty).unwrap(),
            "{}"
        );
    }

    #[test]
    fn post_priority_uses_capitalised_keys_for_its_internal_fields() {
        let priority = PostPriority {
            post_id: "6bdz674pgq767e4jx75w4pf57a".into(),
            channel_id: "qr6kf7ztp7yifxt4wm5xn51bke".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&priority).unwrap();
        assert!(
            json.get("PostId").is_some(),
            "wire key is PostId, not post_id"
        );
        assert!(json.get("ChannelId").is_some());
        assert!(json.get("post_id").is_none());
        // ...while the three pointer fields are snake_case and write null.
        assert_eq!(json["priority"], Value::Null);
        assert_eq!(json["requested_ack"], Value::Null);
    }

    #[test]
    fn a_translation_object_of_null_is_not_the_same_as_an_absent_one() {
        let absent: PostTranslation = serde_json::from_str(r#"{"type":"object"}"#).unwrap();
        assert_eq!(absent.object, None);
        // Check the *key*, not a substring: `"type":"object"` contains "object" too.
        assert!(
            serde_json::to_value(&absent)
                .unwrap()
                .get("object")
                .is_none()
        );

        // Go's RawMessage holds the four bytes `null`, which is not empty, so it survives.
        let explicit: PostTranslation =
            serde_json::from_str(r#"{"object":null,"type":"object"}"#).unwrap();
        assert_eq!(explicit.object, Some(Value::Null));
        assert!(
            crate::utils::go_json_marshal(&explicit)
                .unwrap()
                .contains(r#""object":null"#)
        );
    }

    #[test]
    fn a_translation_object_carries_arbitrary_json() {
        let parsed: PostTranslation =
            serde_json::from_str(r#"{"object":{"a":[1,2]},"type":"object"}"#).unwrap();
        assert_eq!(parsed.object, Some(json!({"a":[1,2]})));
    }

    #[test]
    fn copy_drops_expire_at_and_recipients() {
        let original = PostMetadata {
            expire_at: 1_700_000_000_000,
            recipients: vec!["a".into(), "b".into()],
            redacted_file_count: 7,
            ..Default::default()
        };
        let copied = original.copy();

        assert_eq!(copied.expire_at, 0, "Go drops expire_at");
        assert!(copied.recipients.is_empty(), "Go drops recipients");
        // Everything else survives.
        assert_eq!(copied.redacted_file_count, 7);
    }

    #[test]
    fn copy_does_not_alias_the_original() {
        // Go shares the element pointers here; Rust owns its values. Divergence in the safe
        // direction, documented on `copy`.
        let original = PostMetadata {
            embeds: vec![PostEmbed {
                type_: "link".into(),
                url: "https://example.com".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut copied = original.copy();
        copied.embeds[0].url = "https://changed".into();
        assert_eq!(original.embeds[0].url, "https://example.com");
    }
}

/// Parity tests driven by `fixtures/behaviour_post_metadata.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_post_metadata.json"
        ))
        .unwrap()
    }

    /// Byte-for-byte, and against Go's own round-trip so a lossy case would be caught rather
    /// than papered over. Nothing in this corpus is lossy, which is itself asserted.
    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["wire"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input = case["json"].as_str().unwrap();
            let want = case["roundtrip"].as_str().unwrap();
            assert_eq!(
                input, want,
                "case {name}: Go itself is lossy here, investigate"
            );

            let parsed: Result<PostMetadata, _> = serde_json::from_str(input);

            if name == "embeds_nil_element" {
                // Go's `[]*PostEmbed` accepts a nil element and re-emits it as `null`; our
                // `Vec<PostEmbed>` rejects the document outright. A known, codebase-wide
                // divergence wherever Go has `[]*T` — see D-033. Asserted rather than skipped,
                // so it cannot quietly change without someone noticing.
                assert!(
                    parsed.is_err(),
                    "we now accept a nil element; D-033 may be closable"
                );
                continue;
            }

            let parsed = parsed.unwrap_or_else(|e| panic!("case {name}: {e}"));
            assert_eq!(
                crate::utils::go_json_marshal(&parsed).unwrap(),
                want,
                "case {name}"
            );
        }
    }

    #[test]
    fn post_priority_wire_matches_go() {
        let oracle = oracle();
        let cases = oracle["priority_wire"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input = case["json"].as_str().unwrap();
            let parsed: PostPriority = serde_json::from_str(input).unwrap();
            assert_eq!(
                crate::utils::go_json_marshal(&parsed).unwrap(),
                case["roundtrip"].as_str().unwrap(),
                "case {name}"
            );
        }
    }

    /// `Copy` is asserted on its *output JSON*, which is where the dropped fields show up, plus
    /// the aliasing flags Go recorded — one of which we deliberately do not reproduce.
    #[test]
    fn copy_matches_go() {
        let oracle = oracle();
        let cases = oracle["copy"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input: PostMetadata = serde_json::from_str(case["in"].as_str().unwrap()).unwrap();
            let copied = input.copy();

            assert_eq!(
                crate::utils::go_json_marshal(&copied).unwrap(),
                case["out"].as_str().unwrap(),
                "case {name}"
            );

            // Go records whether it kept these; we must agree, because it is observable JSON.
            assert_eq!(
                copied.expire_at == input.expire_at,
                case["expire_at_survived"].as_bool().unwrap(),
                "case {name}"
            );
            assert_eq!(
                copied.recipients.len() == input.recipients.len(),
                case["recipients_survived"].as_bool().unwrap(),
                "case {name}"
            );

            // The pointer-sharing flags are the one thing we do NOT reproduce: Go aliases the
            // elements, Rust owns them. Assert the oracle still says so, so the divergence
            // cannot quietly disappear.
            if name == "fields_dropped_by_copy" {
                assert!(
                    case["shares_embed_pointer"].as_bool().unwrap(),
                    "Go stopped aliasing embeds; D-034 needs revisiting"
                );
                assert!(!case["shares_priority_pointer"].as_bool().unwrap());
            }
        }
    }
}
