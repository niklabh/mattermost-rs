//! Port of `server/public/model/post_embed.go`.
//!
//! A leaf under `post_metadata.go`, which is a leaf under `post.go`. Three fields, no logic —
//! the whole difficulty is `data`, a bare `any` with `omitempty`.
//!
//! **Go's `omitempty` on an interface tests `IsNil()`, not emptiness.** So `Data: ""` is
//! *emitted* as `""`, and so are `0`, `false` and `{}`; only a nil interface is dropped. A nil
//! *pointer* stored in the interface is non-nil as an interface, so it survives `omitempty` and
//! marshals to `null` — a third distinguishable output state.
//!
//! `Option<Value>` reproduces all three, and reproduces Go's asymmetry on the way back: an
//! explicit `data: null` decodes to a nil interface in Go and to `None` here, so re-marshalling
//! drops the key in **both** languages. That round trip is lossy on purpose; the oracle records
//! Go's own re-marshal so the test asserts against it rather than against the input.
//!
//! Pinned by `fixtures/post_embed.json` and `fixtures/behaviour_post_leaves.json`.

use serde::{Deserialize, Serialize};

/// Port of `model.PostEmbedImage` (post_embed.go:7).
pub const POST_EMBED_IMAGE: &str = "image";
/// Port of `model.PostEmbedMessageAttachment` (post_embed.go:8).
pub const POST_EMBED_MESSAGE_ATTACHMENT: &str = "message_attachment";
/// Port of `model.PostEmbedOpengraph` (post_embed.go:9) — the only type that populates `data`.
pub const POST_EMBED_OPENGRAPH: &str = "opengraph";
/// Port of `model.PostEmbedLink` (post_embed.go:10).
pub const POST_EMBED_LINK: &str = "link";
/// Port of `model.PostEmbedPermalink` (post_embed.go:11).
pub const POST_EMBED_PERMALINK: &str = "permalink";
/// Port of `model.PostEmbedBoards` (post_embed.go:12).
pub const POST_EMBED_BOARDS: &str = "boards";

/// Port of `model.PostEmbed` (post_embed.go:17).
///
/// `type` is Go's defined string type `PostEmbedType`, so `json.Unmarshal` accepts any value
/// into it and an unknown variant round-trips unchanged. Kept as a `String` for the same reason
/// `Channel.Type` is: a Rust enum would turn a forward-compatible read into a parse failure the
/// moment a newer Go server writes a type we have not heard of.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostEmbed {
    #[serde(rename = "type")]
    pub type_: String,

    /// The URL of the embedded content. Used for image and OpenGraph embeds. A non-pointer
    /// `string` with `omitempty`, so it is a `String` with a skip predicate, never an `Option`.
    #[serde(rename = "url", skip_serializing_if = "String::is_empty")]
    pub url: String,

    /// Additional data for the embedded content; only OpenGraph embeds populate it.
    ///
    /// `None` is Go's nil interface and is omitted. `Some(Value::Null)` is the state Go reaches
    /// by storing a typed nil pointer, and writes `"data":null`. Every other `Some` — including
    /// `Some(Value::String("".into()))` and `Some(json!({}))` — is emitted verbatim, because
    /// `omitempty` never looked at the contents.
    #[serde(rename = "data", skip_serializing_if = "Option::is_none")]
    pub data: Option<PostEmbedData>,
}

/// What `PostEmbed.Data` (an `any`) holds.
///
/// Two origins with two different wire behaviours:
///
/// - **Decoded from JSON** ([`PostEmbedData::Json`]) — Go's `any` after `json.Unmarshal`, i.e.
///   maps, slices and scalars. Go re-marshals a `map[string]any` with its keys sorted, which is
///   exactly what `serde_json::Value`'s `BTreeMap` does, so this round-trips byte for byte.
/// - **A typed value the server put there** ([`PostEmbedData::Encoded`]) —
///   `*opengraph.OpenGraph` for an `opengraph` embed (post_metadata.go:639) and `*PreviewPost`
///   for a `permalink` one (:668). Go marshals a struct in **field order**, which a `Value`
///   cannot hold: it would sort `{"type","url","title",…}` alphabetically. So the value is kept
///   as the text `json.Marshal` writes for it, produced by serialising the typed Rust port, and
///   emitted verbatim. This is the half of [D-106] that reaches the wire.
///
/// Go's type assertions (`embed.Data.(*PreviewPost)` in `GetPreviewPost`,
/// `embed.Data.(*opengraph.OpenGraph)` in `getImagesForPost`) succeed only on the typed origin;
/// [`PostEmbedData::decode_typed`] is that assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostEmbedData {
    /// Decoded from JSON.
    Json(serde_json::Value),
    /// A typed Go value, as the compact JSON text `json.Marshal` writes for it.
    Encoded(String),
}

impl PostEmbedData {
    /// Serialise a typed value the way Go stores it in the interface: the struct itself, in
    /// field order.
    pub fn encode<T: Serialize>(value: &T) -> Result<Self, serde_json::Error> {
        serde_json::to_string(value).map(Self::Encoded)
    }

    /// The Go type assertion: `Some` only when the server put a typed value here and it decodes
    /// as `T`. A [`PostEmbedData::Json`] value never asserts, as a `map[string]any` never
    /// asserts to `*PreviewPost` in Go.
    pub fn decode_typed<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        match self {
            Self::Encoded(text) => serde_json::from_str(text).ok(),
            Self::Json(_) => None,
        }
    }

    /// The value as a JSON document, whichever origin it has.
    pub fn to_json_value(&self) -> serde_json::Value {
        match self {
            Self::Json(value) => value.clone(),
            Self::Encoded(text) => serde_json::from_str(text).unwrap_or(serde_json::Value::Null),
        }
    }
}

impl From<serde_json::Value> for PostEmbedData {
    fn from(value: serde_json::Value) -> Self {
        Self::Json(value)
    }
}

impl Serialize for PostEmbedData {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Json(value) => value.serialize(serializer),
            Self::Encoded(text) => serde_json::value::RawValue::from_string(text.clone())
                .map_err(serde::ser::Error::custom)?
                .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for PostEmbedData {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde_json::Value::deserialize(deserializer).map(Self::Json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/post_embed.json");
        let parsed: PostEmbed = serde_json::from_str(raw).unwrap();
        let original: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), original);
    }

    #[test]
    fn omitempty_on_an_interface_drops_only_nil() {
        // Every "empty-looking" value survives, because Go only tested IsNil.
        for value in [json!(""), json!(0), json!(false), json!({}), json!([])] {
            let embed = PostEmbed {
                data: Some(value.clone().into()),
                ..Default::default()
            };
            let encoded = serde_json::to_value(&embed).unwrap();
            assert_eq!(encoded["data"], value);
        }

        // Only the absent case is dropped.
        let encoded = serde_json::to_value(PostEmbed::default()).unwrap();
        assert!(encoded.get("data").is_none());
    }

    #[test]
    fn an_explicit_null_is_a_third_state_on_the_way_out() {
        let embed = PostEmbed {
            data: Some(Value::Null.into()),
            ..Default::default()
        };
        assert_eq!(
            crate::utils::go_json_marshal(&embed).unwrap(),
            r#"{"type":"","data":null}"#
        );
    }

    #[test]
    fn an_explicit_null_collapses_on_the_way_in_exactly_as_go_does() {
        let parsed: PostEmbed = serde_json::from_str(r#"{"type":"","data":null}"#).unwrap();
        assert_eq!(parsed.data, None);
        // ...so re-marshalling drops the key. Go loses it the same way; this is not our bug.
        assert_eq!(
            crate::utils::go_json_marshal(&parsed).unwrap(),
            r#"{"type":""}"#
        );
    }

    /// A typed value keeps its struct's field order on the wire; the same document decoded from
    /// JSON is sorted, as Go's `map[string]any` is.
    #[test]
    fn encoded_data_keeps_field_order_and_json_data_is_sorted() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Typed {
            zebra: i32,
            apple: i32,
        }
        let typed = PostEmbed {
            type_: "opengraph".into(),
            data: Some(PostEmbedData::encode(&Typed { zebra: 1, apple: 2 }).unwrap()),
            ..Default::default()
        };
        assert_eq!(
            crate::utils::go_json_marshal(&typed).unwrap(),
            r#"{"type":"opengraph","data":{"zebra":1,"apple":2}}"#
        );
        let decoded: PostEmbed =
            serde_json::from_str(r#"{"type":"opengraph","data":{"zebra":1,"apple":2}}"#).unwrap();
        assert_eq!(
            crate::utils::go_json_marshal(&decoded).unwrap(),
            r#"{"type":"opengraph","data":{"apple":2,"zebra":1}}"#
        );
        // Go's type assertion holds only for the typed origin.
        assert_eq!(
            typed.data.as_ref().unwrap().decode_typed::<Typed>(),
            Some(Typed { zebra: 1, apple: 2 })
        );
        assert_eq!(decoded.data.as_ref().unwrap().decode_typed::<Typed>(), None);
        assert_eq!(
            typed.data.unwrap().to_json_value(),
            decoded.data.unwrap().to_json_value()
        );
    }

    #[test]
    fn an_unknown_type_round_trips() {
        let raw = r#"{"type":"something_new"}"#;
        let parsed: PostEmbed = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.type_, "something_new");
        assert_eq!(crate::utils::go_json_marshal(&parsed).unwrap(), raw);
    }
}

/// Parity tests driven by `fixtures/behaviour_post_leaves.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_post_leaves.json")).unwrap()
    }

    #[test]
    fn constants_match_go() {
        let oracle = oracle();
        let c = &oracle["embed_constants"];
        assert_eq!(POST_EMBED_IMAGE, c["image"].as_str().unwrap());
        assert_eq!(
            POST_EMBED_MESSAGE_ATTACHMENT,
            c["message_attachment"].as_str().unwrap()
        );
        assert_eq!(POST_EMBED_OPENGRAPH, c["opengraph"].as_str().unwrap());
        assert_eq!(POST_EMBED_LINK, c["link"].as_str().unwrap());
        assert_eq!(POST_EMBED_PERMALINK, c["permalink"].as_str().unwrap());
        assert_eq!(POST_EMBED_BOARDS, c["boards"].as_str().unwrap());
    }

    /// Asserted against Go's **round-trip**, not against its original bytes: `data: null` is
    /// lossy in Go too, so `json` and `roundtrip` differ for exactly one case and matching
    /// `json` there would mean diverging from Go.
    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["embed_wire"].as_array().unwrap();
        assert!(!cases.is_empty());

        let mut saw_lossy = false;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input = case["json"].as_str().unwrap();
            let want = case["roundtrip"].as_str().unwrap();

            let parsed: PostEmbed = serde_json::from_str(input).unwrap();
            assert_eq!(
                crate::utils::go_json_marshal(&parsed).unwrap(),
                want,
                "case {name}"
            );

            if input != want {
                saw_lossy = true;
                assert_eq!(name, "data_typed_nil_pointer", "unexpected lossy case");
            }
        }
        assert!(saw_lossy, "the lossy null case vanished from the corpus");
    }
}
