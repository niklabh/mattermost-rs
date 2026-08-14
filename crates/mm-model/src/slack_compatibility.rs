//! Port of `server/public/model/slack_compatibility.go` — **partial**.
//!
//! The file is 47 lines and is mostly deprecated aliases onto `message_attachment.go`:
//! `SlackAttachment = MessageAttachment`, `SlackAttachmentField = MessageAttachmentField`,
//! `ParseSlackAttachment` and `StringifySlackFieldValue`. Those land with that file. What is
//! here is the one type with behaviour of its own, [`SlackCompatibleBool`], which
//! `MessageAttachmentField.Short` needs.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::utils::go_to_lower;

/// Port of `model.SlackCompatibleBool` (slack_compatibility.go:29).
///
/// Slack lets a bool arrive as a JSON literal (`true`) or as a string (`"true"`), so Go defines
/// an `UnmarshalJSON` that takes both. Two things about it are easy to get wrong, and both were
/// measured rather than read:
///
/// 1. **The case-insensitivity applies only to the quoted form.** Go lowercases the raw token
///    and matches it against `true`, `"true"`, `false` and `"false"` — which reads as though
///    `TRUE` were accepted. It is not: `TRUE` is not a valid JSON token, so `encoding/json`
///    rejects it in the scanner and `UnmarshalJSON` is never called. `"TRUE"` *is* valid JSON
///    and is accepted. So the string form is case-insensitive and the literal form is not.
///
/// 2. **Nothing else is accepted.** Not `1`/`0`, not `"1"`/`"0"`, not `null`, not `"yes"`, and
///    not `" true"` with padding. Unlike `parse_go_bool` (which `Session`'s props use and which
///    takes `1 t T TRUE True`), this one is narrow.
///
/// There is no `MarshalJSON`, so it serialises as a plain JSON bool.
///
/// One deliberate divergence, see [D-037]: Go matches against the **raw** token, so a string
/// spelled with escapes (`"true"`) is rejected even though it decodes to `true`. Serde
/// hands the visitor the decoded string, so this port accepts it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlackCompatibleBool(pub bool);

impl From<bool> for SlackCompatibleBool {
    fn from(b: bool) -> Self {
        Self(b)
    }
}

impl From<SlackCompatibleBool> for bool {
    fn from(b: SlackCompatibleBool) -> Self {
        b.0
    }
}

impl Serialize for SlackCompatibleBool {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(self.0)
    }
}

impl<'de> Deserialize<'de> for SlackCompatibleBool {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct SlackBoolVisitor;

        impl Visitor<'_> for SlackBoolVisitor {
            type Value = SlackCompatibleBool;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(r#"a bool, or the string "true" or "false" in any casing"#)
            }

            fn visit_bool<E: de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(SlackCompatibleBool(v))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                // `strings.ToLower`, not `str::to_lowercase` — see [`go_to_lower`]. No input
                // that reaches here can distinguish them, but the rule is crate-wide.
                match go_to_lower(v).as_str() {
                    "true" => Ok(SlackCompatibleBool(true)),
                    "false" => Ok(SlackCompatibleBool(false)),
                    _ => Err(E::custom(format!(
                        "unmarshal: unable to convert {v} to bool"
                    ))),
                }
            }
        }

        d.deserialize_any(SlackBoolVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialises_as_a_plain_bool() {
        assert_eq!(
            serde_json::to_string(&SlackCompatibleBool(true)).unwrap(),
            "true"
        );
        assert_eq!(
            serde_json::to_string(&SlackCompatibleBool(false)).unwrap(),
            "false"
        );
    }

    #[test]
    fn defaults_to_false() {
        assert_eq!(SlackCompatibleBool::default(), SlackCompatibleBool(false));
    }

    #[test]
    fn converts_both_ways() {
        assert!(bool::from(SlackCompatibleBool::from(true)));
        assert!(!bool::from(SlackCompatibleBool::from(false)));
    }
}

/// Asserted against `fixtures/behaviour_url.json`, produced by
/// `reference/dump/behaviour_url.go` running Go's own `json.Unmarshal`.
#[cfg(test)]
mod go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_url.json")).unwrap()
    }

    /// Every accept, every reject, and the marshalled form — 40 cases including the ones a
    /// lenient port would wave through (`1`, `"1"`, `null`, `"yes"`, `" true"`).
    #[test]
    fn unmarshal_matches_go() {
        let o = oracle();
        for case in o["slack_compatible_bool"].as_array().unwrap() {
            let json = case["json"].as_str().unwrap();
            let want_ok = case["ok"].as_bool().unwrap();
            let want_value = case["value"].as_bool().unwrap();

            // The one measured divergence: Go matches the raw token, we see the decoded
            // string, so an escape-spelled boolean is accepted here and rejected there.
            // Asserted explicitly rather than skipped, so it cannot rot silently — [D-037].
            if json.contains("\\u") {
                let got: SlackCompatibleBool = serde_json::from_str(json).unwrap();
                assert!(!want_ok, "Go unexpectedly accepted {json}");
                // We agree with what the escape *decodes to*, which is exactly the divergence.
                let decoded: String = serde_json::from_str(json).unwrap();
                assert_eq!(got.0, go_to_lower(&decoded) == "true", "{json}");
                continue;
            }

            match serde_json::from_str::<SlackCompatibleBool>(json) {
                Ok(got) => {
                    assert!(want_ok, "{json}: we accept, Go rejects");
                    assert_eq!(got.0, want_value, "{json}: value");
                    assert_eq!(
                        serde_json::to_string(&got).unwrap(),
                        case["marshalled"].as_str().unwrap(),
                        "{json}: marshalled"
                    );
                }
                Err(_) => assert!(!want_ok, "{json}: we reject, Go accepts"),
            }
        }
    }

    /// The asymmetry, called out on its own: the **string** form is case-insensitive and the
    /// **literal** form is not, because a bare `TRUE` never reaches `UnmarshalJSON`.
    #[test]
    fn only_the_quoted_form_is_case_insensitive() {
        let o = oracle();
        let by_json = |needle: &str| {
            o["slack_compatible_bool"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["json"].as_str().unwrap() == needle)
                .unwrap_or_else(|| panic!("{needle} missing from the corpus"))
                .clone()
        };

        assert!(!by_json("TRUE")["ok"].as_bool().unwrap());
        assert!(by_json(r#""TRUE""#)["ok"].as_bool().unwrap());

        assert!(serde_json::from_str::<SlackCompatibleBool>("TRUE").is_err());
        assert_eq!(
            serde_json::from_str::<SlackCompatibleBool>(r#""TRUE""#).unwrap(),
            SlackCompatibleBool(true)
        );
    }
}
