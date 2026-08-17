//! Port of `model/post_info.go` (post_info.go:1–15).
//!
//! One wire struct, eight fields, no methods and no constructor — the response body of
//! `GET /posts/{post_id}/info`, which tells a client what it is allowed to see about the post's
//! home before it fetches the post itself.
//!
//! Two things are worth stating because they are unusual for this package rather than because
//! they are complicated:
//!
//! - **Not one field carries `omitempty`.** The zero value is eight keys, so an empty response
//!   is `{"channel_id":"", …, "has_joined_team":false}` and never `{}`. Nothing here is
//!   `Option`, and nothing is skipped on serialize.
//! - **`channel_type` is Go's `ChannelType`, a defined string type**, so `json.Unmarshal`
//!   accepts any string into it — `NOT_A_TYPE` decodes without complaint. Modelled as a `String`
//!   for the reason [`crate::channel::Channel`] gives: a closed enum would turn a value written
//!   by a newer Go server into a hard decode error. Unlike `Channel`, there is no `IsValid` here
//!   to enforce the accepted set afterwards, so the string really is the whole contract.

use serde::{Deserialize, Serialize};

/// Port of `model.PostInfo` (post_info.go:6).
///
/// `#[serde(default)]` for [D-043]: Go leaves an absent key at its zero value, and the Go server
/// itself sends partial objects here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PostInfo {
    #[serde(rename = "channel_id")]
    pub channel_id: String,

    /// Go's `ChannelType`. See the module docs for why this is not an enum; the accepted values
    /// are the `CHANNEL_TYPE_*` constants in [`crate::channel`].
    #[serde(rename = "channel_type")]
    pub channel_type: String,

    #[serde(rename = "channel_display_name")]
    pub channel_display_name: String,

    #[serde(rename = "has_joined_channel")]
    pub has_joined_channel: bool,

    #[serde(rename = "team_id")]
    pub team_id: String,

    /// A plain `string` in Go, **not** a defined type — unlike `channel_type` two fields up.
    #[serde(rename = "team_type")]
    pub team_type: String,

    #[serde(rename = "team_display_name")]
    pub team_display_name: String,

    #[serde(rename = "has_joined_team")]
    pub has_joined_team: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE};

    #[test]
    fn the_zero_value_is_eight_keys() {
        // No omitempty anywhere, so an empty response is never `{}`.
        assert_eq!(
            serde_json::to_string(&PostInfo::default()).unwrap(),
            r#"{"channel_id":"","channel_type":"","channel_display_name":"","has_joined_channel":false,"team_id":"","team_type":"","team_display_name":"","has_joined_team":false}"#
        );
    }

    #[test]
    fn an_unknown_channel_type_decodes() {
        // The whole reason channel_type is a String: a newer Go server's value must not be a
        // decode error.
        let decoded: PostInfo = serde_json::from_str(r#"{"channel_type":"NOT_A_TYPE"}"#).unwrap();
        assert_eq!(decoded.channel_type, "NOT_A_TYPE");

        for known in [CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE] {
            let doc = format!(r#"{{"channel_type":"{known}"}}"#);
            let decoded: PostInfo = serde_json::from_str(&doc).unwrap();
            assert_eq!(decoded.channel_type, known);
        }
    }

    #[test]
    fn a_partial_object_decodes() {
        let decoded: PostInfo = serde_json::from_str(r#"{"channel_id":"c1"}"#).unwrap();
        assert_eq!(decoded.channel_id, "c1");
        assert!(decoded.team_id.is_empty() && !decoded.has_joined_team);
    }
}

/// Parity tests driven by `fixtures/behaviour_post_info.json` — Go's own answers. Also covers
/// `post_attributes.go`, which is two constants and has no fixture of its own.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::post_attributes::{
        POST_ATTRIBUTES_PROPERTY_GROUP_NAME, POST_ATTRIBUTES_PROPERTY_GROUP_SCHEMA_VERSION,
    };
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_post_info.json")).unwrap()
    }

    /// The one document Go accepts and we reject: `null` into a scalar. See [D-057].
    const NULL_SCALAR_ONLY: &str = "null_scalars";

    #[test]
    fn the_post_attributes_constants_match_go() {
        let oracle = oracle();
        let constants = &oracle["constants"];
        assert_eq!(
            constants["PostAttributesPropertyGroupName"]
                .as_str()
                .unwrap(),
            POST_ATTRIBUTES_PROPERTY_GROUP_NAME
        );
        assert_eq!(
            constants["PostAttributesPropertyGroupSchemaVersion"]
                .as_i64()
                .unwrap(),
            POST_ATTRIBUTES_PROPERTY_GROUP_SCHEMA_VERSION
        );
    }

    #[test]
    fn the_wire_format_matches_go() {
        let oracle = oracle();
        let cases = oracle["wire"].as_array().unwrap();
        assert!(!cases.is_empty());

        let mut checked = 0;
        for case in cases {
            let name = case["name"].as_str().unwrap();
            assert!(!case["panicked"].as_bool().unwrap(), "{name}: Go panicked");
            assert!(case["err"].is_null(), "{name}: Go failed to decode");

            if name == NULL_SCALAR_ONLY {
                continue;
            }

            let decoded: PostInfo = match case["in"].as_str().unwrap() {
                // The zero-value row has no input document; it pins the marshal side alone.
                "" => PostInfo::default(),
                doc => serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}")),
            };
            // Byte-for-byte, which pins the field order and Go's HTML escaping.
            assert_eq!(
                go_json_marshal(&decoded).unwrap(),
                case["out"].as_str().unwrap(),
                "{name}"
            );
            if let Some(channel_type) = case["channel_type"].as_str() {
                assert_eq!(decoded.channel_type, channel_type, "{name}: channel_type");
            }
            checked += 1;
        }
        assert_eq!(checked, cases.len() - 1, "every case but the null one");
    }

    /// [D-057] again, on a type with no slices at all: Go leaves a scalar untouched on `null`,
    /// serde rejects the document. Asserted so closing the debt fails this test.
    #[test]
    fn a_null_scalar_is_accepted_by_go_and_rejected_here() {
        let oracle = oracle();
        let case = oracle["wire"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"].as_str() == Some(NULL_SCALAR_ONLY))
            .expect(NULL_SCALAR_ONLY);

        let doc = case["in"].as_str().unwrap();
        assert!(case["err"].is_null(), "Go rejected {doc}");
        assert_eq!(
            case["out"].as_str().unwrap(),
            go_json_marshal(&PostInfo::default()).unwrap(),
            "Go decoded it to the zero value"
        );
        assert!(
            serde_json::from_str::<PostInfo>(doc).is_err(),
            "{doc}: we accepted it, so [D-057] can be closed"
        );
    }
}

/// Serialization parity against `fixtures/post_info.json` — every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/post_info.json");
        let decoded: PostInfo = serde_json::from_str(raw).unwrap();

        // The type has no omitempty, so a zero-valued fixture would still carry every key and
        // the round trip would prove nothing. Check the values, not just the keys.
        assert!(!decoded.channel_id.is_empty() && !decoded.team_id.is_empty());
        assert!(decoded.has_joined_channel && decoded.has_joined_team);

        let ours: serde_json::Value = serde_json::to_value(&decoded).unwrap();
        let theirs: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(ours, theirs);
    }
}
