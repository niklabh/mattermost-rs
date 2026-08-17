//! Port of `server/public/model/message_attachment.go` — the whole file, 292 lines.
//!
//! The interactive-message attachment format Mattermost inherited from Slack. Two of its fields
//! are bare `any`s (`ts` and each field's `value`) and their validators check the **Go dynamic
//! type**, which JSON cannot express — see [`MessageAttachment::is_valid`] and [D-039].
//!
//! `IsValid` here accumulates every failure into a [`MultiError`] rather than returning the
//! first, the same convention `integration_action.go` uses.

use serde::{Deserialize, Serialize};

use crate::integration_action::PostAction;
use crate::post::{POST_PROPS_ATTACHMENTS, POST_TYPE_MESSAGE_ATTACHMENT, Post};
use crate::slack_compatibility::SlackCompatibleBool;
use crate::utils::{MultiError, go_format_v, is_valid_http_url};

/// The colour words [`MessageAttachment::is_valid`] accepts besides a hex colour.
///
/// **Three words, not the six `PostAction::style` takes.** `primary` and `default` are valid
/// action styles and invalid attachment colours; both share the six-digit hex regex.
const VALID_ATTACHMENT_COLORS: [&str; 3] = ["good", "warning", "danger"];

/// Go's `linkWithTextRegex` (message_attachment.go:16).
fn link_with_text_regex() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        #[allow(clippy::expect_used)]
        regex::Regex::new(r"<([^<\|]+)\|([^>]+)>").expect("literal regex is valid")
    });
    &RE
}

/// Go's `hexColorRegex` (message_attachment.go:17). Six digits only — `channel.rs` has a
/// different one that also takes three, so `#abc` is a valid channel banner colour and an
/// invalid attachment colour.
///
/// `PostAction::is_valid` shares it: Go declares it here and uses it from
/// `integration_action.go` too.
pub(crate) fn hex_color_regex() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        #[allow(clippy::expect_used)]
        regex::Regex::new(r"^#[0-9a-fA-F]{6}$").expect("literal regex is valid")
    });
    &RE
}

// --- MessageAttachment -----------------------------------------------------------------------

/// Port of `model.MessageAttachment` (message_attachment.go:20).
///
/// **Only `actions` carries `omitempty`.** Every other key is always present, so a zero
/// attachment serialises with `"fields":null` and `"ts":null` and no `actions` key at all.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageAttachment {
    #[serde(rename = "id", default)]
    pub id: i64,

    #[serde(rename = "fallback", default)]
    pub fallback: String,

    /// `good`, `warning`, `danger`, or a six-digit hex colour.
    #[serde(rename = "color", default)]
    pub color: String,

    #[serde(rename = "pretext", default)]
    pub pretext: String,

    #[serde(rename = "author_name", default)]
    pub author_name: String,

    #[serde(rename = "author_link", default)]
    pub author_link: String,

    #[serde(rename = "author_icon", default)]
    pub author_icon: String,

    #[serde(rename = "title", default)]
    pub title: String,

    #[serde(rename = "title_link", default)]
    pub title_link: String,

    #[serde(rename = "text", default)]
    pub text: String,

    /// No `omitempty`, so a nil slice is `null` and an empty one is `[]`.
    #[serde(rename = "fields", default)]
    pub fields: Option<Vec<MessageAttachmentField>>,

    #[serde(rename = "image_url", default)]
    pub image_url: String,

    #[serde(rename = "thumb_url", default)]
    pub thumb_url: String,

    #[serde(rename = "footer", default)]
    pub footer: String,

    #[serde(rename = "footer_icon", default)]
    pub footer_icon: String,

    /// Go's comment says "either a string or an int64", and `IsValid` enforces exactly that on
    /// the **Go** type. A `null` and a missing key are the same thing here — no `omitempty`, so
    /// the key is always written.
    #[serde(rename = "ts", default)]
    pub timestamp: serde_json::Value,

    /// The only field with `omitempty`, which drops a nil slice and an empty one alike.
    #[serde(rename = "actions", default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<PostAction>,
}

impl MessageAttachment {
    /// Port of `(*MessageAttachment).IsValid` (message_attachment.go:40).
    ///
    /// Accumulates every failure. The order is: colour, author link (two independent checks),
    /// author icon, title link (two checks), **fields**, image URL, thumb URL, footer icon,
    /// timestamp, then actions. Note the fields loop sits before the three URL checks, and that
    /// field failures are appended **unprefixed** while action failures get
    /// `action at index N is invalid:`.
    ///
    /// The URL checks are [`is_valid_http_url`] alone — unlike `PostAction`'s integration URL,
    /// a `/plugins/` path is **not** accepted here.
    ///
    /// `timestamp` and each field's `value` are validated against a Go dynamic type that JSON
    /// cannot produce; see [`Self::timestamp_is_valid`] and [D-039].
    pub fn is_valid(&self) -> Result<(), MultiError> {
        let mut errs = MultiError::new();

        if !self.color.is_empty()
            && !VALID_ATTACHMENT_COLORS.contains(&self.color.as_str())
            && !hex_color_regex().is_match(&self.color)
        {
            errs.push(format!(
                "invalid style '{}' - must be one of [good, warning, danger] or a hex color",
                self.color
            ));
        }

        if !self.author_link.is_empty() {
            if self.author_name.is_empty() {
                errs.push("author link cannot be set without author name");
            }
            if !is_valid_http_url(&self.author_link) {
                errs.push("invalid author link URL");
            }
        }

        if !self.author_icon.is_empty() && !is_valid_http_url(&self.author_icon) {
            errs.push("invalid author icon URL");
        }

        if !self.title_link.is_empty() {
            if self.title.is_empty() {
                errs.push("title link cannot be set without title");
            }
            if !is_valid_http_url(&self.title_link) {
                errs.push("invalid title link URL");
            }
        }

        for field in self.fields.iter().flatten() {
            if let Err(e) = field.is_valid() {
                errs.extend(e);
            }
        }

        if !self.image_url.is_empty() && !is_valid_http_url(&self.image_url) {
            errs.push("invalid image URL");
        }

        if !self.thumb_url.is_empty() && !is_valid_http_url(&self.thumb_url) {
            errs.push("invalid thumb URL");
        }

        if !self.footer_icon.is_empty() && !is_valid_http_url(&self.footer_icon) {
            errs.push("invalid footer icon URL");
        }

        if !self.timestamp_is_valid() {
            errs.push("timestamp must be either a string or int64");
        }

        for (i, action) in self.actions.iter().enumerate() {
            if let Err(e) = action.is_valid() {
                errs.extend(e.prefixed(&format!("action at index {i} is invalid:")));
            }
        }

        errs.into_result()
    }

    /// Go checks `switch s.Timestamp.(type) { case string, int64: }`, and `encoding/json`
    /// decodes **every** JSON number into a `float64` — so a `ts` that arrived over the wire is
    /// valid only when it is a string or absent. `"ts": 123` is invalid. See [D-039].
    fn timestamp_is_valid(&self) -> bool {
        matches!(
            self.timestamp,
            serde_json::Value::Null | serde_json::Value::String(_)
        )
    }

    /// Port of `(*MessageAttachment).Equals` (message_attachment.go:112). Compares all 17
    /// fields — unlike `PostAction::equals`, this one is complete.
    ///
    /// Go compares `Timestamp` with `==` on two `any`s, which distinguishes the dynamic type.
    /// Both sides of a real comparison come from JSON and are therefore both `float64`, so
    /// `1` and `1.0` are equal — [`crate::utils::json_values_equal_like_go`] reproduces that,
    /// where a plain `serde_json::Value` comparison would not.
    pub fn equals(&self, input: &MessageAttachment) -> bool {
        if self.id != input.id
            || self.fallback != input.fallback
            || self.color != input.color
            || self.pretext != input.pretext
            || self.author_name != input.author_name
            || self.author_link != input.author_link
            || self.author_icon != input.author_icon
            || self.title != input.title
            || self.title_link != input.title_link
            || self.text != input.text
            || self.image_url != input.image_url
            || self.thumb_url != input.thumb_url
            || self.footer != input.footer
            || self.footer_icon != input.footer_icon
        {
            return false;
        }

        let ours = self.fields.as_deref().unwrap_or_default();
        let theirs = input.fields.as_deref().unwrap_or_default();
        if ours.len() != theirs.len() {
            return false;
        }
        for (a, b) in ours.iter().zip(theirs.iter()) {
            if !a.equals(b) {
                return false;
            }
        }

        if self.actions.len() != input.actions.len() {
            return false;
        }
        for (a, b) in self.actions.iter().zip(input.actions.iter()) {
            if !a.equals(b) {
                return false;
            }
        }

        crate::utils::json_values_equal_like_go(&self.timestamp, &input.timestamp)
    }
}

/// Port of `model.MessageAttachmentField` (message_attachment.go:196). No field carries
/// `omitempty`, so all three keys are always present.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageAttachmentField {
    #[serde(rename = "title", default)]
    pub title: String,

    /// Go's comment and validator say "a string or an int". As with
    /// [`MessageAttachment::timestamp`], a JSON number decodes to `float64` and is therefore
    /// **invalid** — see [D-039].
    #[serde(rename = "value", default)]
    pub value: serde_json::Value,

    /// Accepts `true`, `false`, `"true"` or `"false"` inbound; always a plain bool outbound.
    #[serde(rename = "short", default)]
    pub short: SlackCompatibleBool,
}

impl MessageAttachmentField {
    /// Port of `(*MessageAttachmentField).IsValid` (message_attachment.go:202).
    pub fn is_valid(&self) -> Result<(), MultiError> {
        let mut errs = MultiError::new();
        if !matches!(
            self.value,
            serde_json::Value::Null | serde_json::Value::String(_)
        ) {
            errs.push("value must be either a string or int");
        }
        errs.into_result()
    }

    /// Port of `(*MessageAttachmentField).Equals` (message_attachment.go:217).
    ///
    /// **Go panics whenever either `Value` is nil** — it calls
    /// `reflect.ValueOf(input.Value).Type()`, and `Type()` on the zero `reflect.Value` panics.
    /// Since a field with no `value` key decodes to exactly that, comparing two ordinary
    /// attachments crashes the Go server. Ours compares `Value::Null` normally; see [D-039].
    pub fn equals(&self, input: &MessageAttachmentField) -> bool {
        self.title == input.title
            && crate::utils::json_values_equal_like_go(&self.value, &input.value)
            && self.short == input.short
    }
}

/// Port of `model.StringifyMessageAttachmentFieldValue` (message_attachment.go:235).
///
/// Drops nil attachments and nil fields — which a `Vec<T>` cannot hold anyway — and rewrites
/// every non-nil field value as its Go `%v` rendering. That is Go's formatting, not JSON's:
/// see [`go_format_v`]. A nil value is left nil rather than becoming `"<nil>"`.
///
/// Go mutates the attachments in place *and* returns a filtered slice; this takes ownership and
/// returns the result, which is the same thing for every Go call site (they all reassign).
pub fn stringify_message_attachment_field_value(
    attachments: Vec<MessageAttachment>,
) -> Vec<MessageAttachment> {
    attachments
        .into_iter()
        .map(|mut attachment| {
            if let Some(fields) = attachment.fields.as_mut() {
                for field in fields.iter_mut() {
                    if !field.value.is_null() {
                        field.value = serde_json::Value::String(go_format_v(&field.value));
                    }
                }
            }
            attachment
        })
        .collect()
}

/// Port of `model.ParseMessageAttachment` (message_attachment.go:262).
///
/// Sets the post type when it is empty, rewrites Slack-style links in each attachment's `text`
/// and `pretext` and in every **string** field value, and writes the result to
/// `props.attachments`. It does *not* stringify non-string field values — that is
/// [`stringify_message_attachment_field_value`]'s job and the two are separate passes.
pub fn parse_message_attachment(post: &mut Post, attachments: Vec<MessageAttachment>) {
    if post.post_type.is_empty() {
        post.post_type = POST_TYPE_MESSAGE_ATTACHMENT.to_string();
    }

    let parsed: Vec<MessageAttachment> = attachments
        .into_iter()
        .map(|mut attachment| {
            attachment.text = parse_slack_links_to_markdown(&attachment.text);
            attachment.pretext = parse_slack_links_to_markdown(&attachment.pretext);

            if let Some(fields) = attachment.fields.as_mut() {
                for field in fields.iter_mut() {
                    if let serde_json::Value::String(s) = &field.value {
                        field.value = serde_json::Value::String(parse_slack_links_to_markdown(s));
                    }
                }
            }
            attachment
        })
        .collect();

    let value = serde_json::to_value(&parsed).unwrap_or(serde_json::Value::Null);
    post.add_prop(POST_PROPS_ATTACHMENTS, value);
}

/// Port of `model.ParseSlackLinksToMarkdown` (message_attachment.go:290).
///
/// Rewrites `<url|text>` as `[text](url)`. The URL group rejects `<` and `|`; the text group
/// rejects `>` but **accepts `|`**, so `<a|b|c>` becomes `[b|c](a)`. Neither group matches
/// empty, so `<a|>` and `<|b>` are left alone. Nothing is escaped, so a `]` in the text or a
/// `)` in the URL produces malformed markdown — reproduced as-is.
pub fn parse_slack_links_to_markdown(text: &str) -> String {
    link_with_text_regex()
        .replace_all(text, "[${2}](${1})")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_attachment_keeps_every_key_but_actions() {
        let s = serde_json::to_string(&MessageAttachment::default()).unwrap();
        assert!(s.contains(r#""fields":null"#), "{s}");
        assert!(s.contains(r#""ts":null"#), "{s}");
        assert!(!s.contains("actions"), "{s}");
    }

    #[test]
    fn a_zero_field_keeps_all_three_keys() {
        assert_eq!(
            serde_json::to_string(&MessageAttachmentField::default()).unwrap(),
            r#"{"title":"","value":null,"short":false}"#
        );
    }

    #[test]
    fn a_json_number_timestamp_is_invalid() {
        let a: MessageAttachment = serde_json::from_str(r#"{"ts":123}"#).unwrap();
        let err = a.is_valid().unwrap_err();
        assert_eq!(
            err.messages(),
            ["timestamp must be either a string or int64"]
        );
    }

    #[test]
    fn a_string_timestamp_is_valid() {
        let a: MessageAttachment = serde_json::from_str(r#"{"ts":"123"}"#).unwrap();
        assert!(a.is_valid().is_ok());
    }

    #[test]
    fn the_colour_list_is_not_the_action_style_list() {
        let mut a = MessageAttachment {
            color: "primary".into(),
            ..Default::default()
        };
        assert!(a.is_valid().is_err());
        a.color = "good".into();
        assert!(a.is_valid().is_ok());
    }

    #[test]
    fn a_plugin_path_is_not_a_valid_attachment_url() {
        let a = MessageAttachment {
            author_name: "an".into(),
            author_link: "/plugins/x".into(),
            ..Default::default()
        };
        assert_eq!(
            a.is_valid().unwrap_err().messages(),
            ["invalid author link URL"]
        );
    }

    #[test]
    fn comparing_fields_with_null_values_does_not_panic_here() {
        let a = MessageAttachmentField::default();
        assert!(a.equals(&MessageAttachmentField::default()));
    }

    #[test]
    fn parse_slack_links_leaves_an_empty_text_group_alone() {
        assert_eq!(parse_slack_links_to_markdown("<a|>"), "<a|>");
        assert_eq!(parse_slack_links_to_markdown("<a|b>"), "[b](a)");
    }
}

/// Asserted against `fixtures/behaviour_message_attachment.json`, produced by
/// `reference/dump/behaviour_message_attachment.go`.
#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_message_attachment.json"
        ))
        .unwrap()
    }

    fn s(v: &Value, key: &str) -> String {
        v.get(key).unwrap().as_str().unwrap().to_string()
    }

    fn b(v: &Value, key: &str) -> bool {
        v.get(key).unwrap().as_bool().unwrap()
    }

    fn messages(v: &Value, key: &str) -> Vec<String> {
        serde_json::from_value(v.get(key).unwrap().clone()).unwrap()
    }

    /// Byte-exact through `go_json_marshal`, for both types.
    #[test]
    fn the_wire_format_matches_go() {
        let o = oracle();
        for case in o.get("wire").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            let go_json = s(case, "json");
            let go_roundtrip = s(case, "roundtrip");

            let ours = if name.starts_with("field_") {
                let v: MessageAttachmentField = serde_json::from_str(&go_json).unwrap();
                crate::utils::go_json_marshal(&v).unwrap()
            } else {
                let v: MessageAttachment = serde_json::from_str(&go_json).unwrap();
                crate::utils::go_json_marshal(&v).unwrap()
            };
            assert_eq!(ours, go_roundtrip, "{name}");
        }
    }

    /// The headline divergence, isolated: the same JSON validates differently in Go depending
    /// on whether the struct was decoded or built in Go code, because `encoding/json` makes
    /// every number a `float64` and neither validator accepts one. This corpus is the decoded
    /// direction, which is the one a server actually takes.
    #[test]
    fn is_valid_matches_go_for_json_decoded_values() {
        let o = oracle();
        for case in o.get("go_type_vs_json").unwrap().as_array().unwrap() {
            let json = s(case, "json");
            let want = messages(case, "messages");
            let a: MessageAttachment = serde_json::from_str(&json).unwrap();
            let got = match a.is_valid() {
                Ok(()) => Vec::new(),
                Err(e) => e.messages().to_vec(),
            };
            assert_eq!(got, want, "{json}");
        }
    }

    /// 42 cases. Every case whose `ts` or field `value` holds a Go-native type that JSON cannot
    /// express is skipped here and covered by the decoded corpus above — see [D-039].
    #[test]
    fn is_valid_matches_go() {
        let o = oracle();
        // These build a Go value whose dynamic type does not survive a marshal/unmarshal, so
        // the recorded JSON cannot reproduce the input. Named explicitly rather than guessed.
        let go_typed = [
            "field_value_int",
            "field_value_int64_is_invalid",
            "field_value_float_is_invalid",
            "field_value_bool_is_invalid",
            "two_bad_fields",
            "ts_int64",
            "ts_int_is_invalid",
            "ts_float_is_invalid",
            "ts_bool_is_invalid",
            "many_failures",
        ];

        let mut checked = 0;
        for case in o.get("is_valid").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            if go_typed.contains(&name.as_str()) {
                continue;
            }
            let a: MessageAttachment =
                serde_json::from_value(case.get("attachment").unwrap().clone()).unwrap();
            let got = match a.is_valid() {
                Ok(()) => Vec::new(),
                Err(e) => e.messages().to_vec(),
            };
            assert_eq!(got, messages(case, "messages"), "{name}");

            if !got.is_empty() {
                assert_eq!(
                    a.is_valid().unwrap_err().to_string(),
                    s(case, "error"),
                    "{name}"
                );
            }
            checked += 1;
        }
        assert!(checked > 25, "too few cases survived the filter: {checked}");
    }

    #[test]
    fn field_is_valid_matches_go_for_json_expressible_types() {
        let o = oracle();
        for case in o.get("field_is_valid").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            // `int` vs `int64` vs `float64` is a Go-only distinction; JSON has one number type,
            // and Go rejects it because a decoded number is always float64.
            let value = match name.as_str() {
                "nil" => Value::Null,
                "string" => Value::String("x".into()),
                "empty_string" => Value::String(String::new()),
                "float64" | "int64" | "int" | "float64_whole" => serde_json::json!(1.5),
                "bool" => Value::Bool(true),
                "slice" => serde_json::json!([1]),
                "map" => serde_json::json!({"a": 1}),
                other => panic!("unknown case {other}"),
            };
            let want = if matches!(name.as_str(), "int" | "float64_whole") {
                // Go accepts a Go-native `int` and rejects the float64 a decode produces; the
                // JSON-expressible answer is the rejection. Asserted from the decoded corpus.
                vec!["value must be either a string or int".to_string()]
            } else {
                messages(case, "messages")
            };

            let f = MessageAttachmentField {
                title: "t".into(),
                value,
                ..Default::default()
            };
            let got = match f.is_valid() {
                Ok(()) => Vec::new(),
                Err(e) => e.messages().to_vec(),
            };
            assert_eq!(got, want, "{name}");
        }
    }

    /// The comparison a server actually performs: both sides decoded from JSON, where Go has
    /// collapsed every number to `float64`. `1` and `1.0` are therefore equal — which a plain
    /// `serde_json::Value` comparison gets wrong.
    #[test]
    fn equals_matches_go_for_json_decoded_values() {
        let o = oracle();
        for case in o.get("equals_from_json").unwrap().as_array().unwrap() {
            let a_json = s(case, "a");
            let b_json = s(case, "b");

            let a: MessageAttachment = serde_json::from_str(&a_json).unwrap();
            let b_att: MessageAttachment = serde_json::from_str(&b_json).unwrap();

            // Go panics whenever a field's Value is nil; ours compares it. [D-039].
            if b(case, "panicked") {
                a.equals(&b_att);
                continue;
            }
            assert_eq!(a.equals(&b_att), b(case, "equals"), "{a_json} vs {b_json}");
        }
    }

    /// Go's `%v` for every value kind a decoded field can hold, including the container cases:
    /// `<nil>` inside a slice, `map[k:v]` with sorted keys, and a float via `%g`.
    #[test]
    fn go_format_v_matches_go() {
        let o = oracle();
        for case in o.get("sprintf_v_from_json").unwrap().as_array().unwrap() {
            let json = s(case, "json");
            let value: Value = serde_json::from_str(&json).unwrap();
            if case.get("formatted").unwrap().is_null() {
                continue; // a bare JSON null never reaches Sprintf
            }
            assert_eq!(go_format_v(&value), s(case, "formatted"), "{json}");
        }
    }

    /// 36 floats through Go's `%g`. Rust's `Display` never uses exponent form and its
    /// `LowerExp` always does, so neither is substitutable.
    #[test]
    fn go_format_float_matches_go() {
        let o = oracle();
        for case in o.get("go_format_v_floats").unwrap().as_array().unwrap() {
            let json = s(case, "json");
            let value: Value = serde_json::from_str(&json).unwrap();
            let f = value.as_f64().unwrap();
            assert_eq!(
                crate::utils::go_format_float(f),
                s(case, "formatted"),
                "{json}"
            );
        }
    }

    #[test]
    fn stringify_matches_go() {
        let o = oracle();
        let case = &o.get("stringify").unwrap().as_array().unwrap()[0];

        // Go's input has two nil attachments and one nil field, which a Vec cannot hold; the
        // filtering is therefore already done for us. [D-033].
        let input = vec![
            MessageAttachment {
                text: "keep".into(),
                fields: Some(vec![
                    MessageAttachmentField {
                        title: "a".into(),
                        value: serde_json::json!("string"),
                        ..Default::default()
                    },
                    MessageAttachmentField {
                        title: "b".into(),
                        value: serde_json::json!(3),
                        ..Default::default()
                    },
                    MessageAttachmentField {
                        title: "c".into(),
                        value: serde_json::json!(1.5),
                        ..Default::default()
                    },
                    MessageAttachmentField {
                        title: "d".into(),
                        value: serde_json::json!(true),
                        ..Default::default()
                    },
                    MessageAttachmentField {
                        title: "e".into(),
                        value: Value::Null,
                        ..Default::default()
                    },
                    MessageAttachmentField {
                        title: "f".into(),
                        value: serde_json::json!([1, "x"]),
                        ..Default::default()
                    },
                    MessageAttachmentField {
                        title: "g".into(),
                        value: serde_json::json!({"z": 1, "a": 2}),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
            MessageAttachment {
                text: "second".into(),
                ..Default::default()
            },
        ];

        let out = stringify_message_attachment_field_value(input);
        assert_eq!(
            out.len(),
            case.get("out_len").unwrap().as_u64().unwrap() as usize
        );
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            *case.get("out").unwrap()
        );
    }

    #[test]
    fn parse_slack_links_matches_go() {
        let o = oracle();
        for (input, want) in o.get("parse_slack_links").unwrap().as_object().unwrap() {
            assert_eq!(
                parse_slack_links_to_markdown(input),
                want.as_str().unwrap(),
                "{input:?}"
            );
        }
    }

    #[test]
    fn parse_message_attachment_matches_go() {
        let o = oracle();
        for case in o.get("parse_attachment").unwrap().as_array().unwrap() {
            let name = s(case, "name");
            // The nil-element case is unrepresentable here; Go keeps nil *fields* in place and
            // drops nil *attachments*. [D-033].
            if name == "nil_fields_are_kept_but_skipped" {
                continue;
            }

            let attachments: Vec<MessageAttachment> = match name.as_str() {
                "empty_type_becomes_slack_attachment" => vec![MessageAttachment {
                    text: "<https://a.com|A>".into(),
                    pretext: "<https://b.com|B>".into(),
                    ..Default::default()
                }],
                "existing_type_is_kept" => vec![MessageAttachment {
                    text: "plain".into(),
                    ..Default::default()
                }],
                "nil_attachments_are_dropped" => vec![MessageAttachment {
                    text: "kept".into(),
                    ..Default::default()
                }],
                _ => Vec::new(),
            };

            let mut post = Post {
                post_type: s(case, "in_type"),
                ..Default::default()
            };
            parse_message_attachment(&mut post, attachments);

            assert_eq!(post.post_type, s(case, "out_type"), "{name}: type");
            assert_eq!(
                serde_json::to_value(post.props.as_ref().unwrap()).unwrap(),
                *case.get("out_props").unwrap(),
                "{name}: props"
            );
        }
    }
}
