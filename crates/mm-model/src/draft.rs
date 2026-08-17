//! Port of `server/public/model/draft.go` (110 lines) — **whole file**.
//!
//! A `Draft` is an unsent message the server persists per (user, channel, root) so it survives a
//! client restart. It reads like a trimmed-down [`Post`](crate::post::Post) and shares three of
//! its constants, but it is not a subset — five things differ, and each is measured against Go
//! rather than reasoned about:
//!
//! 1. **The message-length check runs first.** `Post::is_valid` checks the id and the timestamps
//!    before it looks at the message; [`Draft::is_valid`] checks the message and *then* calls
//!    [`Draft::base_is_valid`]. A wholly zero draft with an over-long message reports
//!    `message_length`, where the same object as a post would report `id`.
//! 2. **`Where` is `Drafts.IsValid` — plural** — on every branch, `base_is_valid`'s included.
//!    `Post` uses the singular. It is off the wire (`json:"-"`) but it reaches the server log.
//! 3. **The details are `channelid=…`, not `id=…`**, and the three id branches carry no detail
//!    at all.
//! 4. **Nothing validates `type`.** `Post` enforces an accepted set plus the `custom_` prefix; a
//!    draft's type is stored verbatim, `system_nope` and a thousand characters included.
//! 5. **`priority` is a bare `StringInterface`**, not the typed `PostPriority` that
//!    [`PostMetadata`] carries. It is measured by the props cap and never validated, so a draft
//!    can hold a priority no post could.
//!
//! Field shapes, all confirmed by wire probe:
//!
//! | Go | wire when nil | wire when empty |
//! |---|---|---|
//! | `Props StringInterface` (no `omitempty`) | `"props":null` | `"props":{}` |
//! | `FileIds StringArray,omitempty` | absent | absent |
//! | `Priority StringInterface,omitempty` | absent | absent |
//! | `Metadata *PostMetadata,omitempty` | absent | `"metadata":{}` |
//!
//! The last row is the one a reader is likely to get wrong: `omitempty` on a **pointer** tests
//! the pointer, so an empty-but-allocated metadata still reaches the client as `{}`.
//!
//! `Draft.Props` is guarded by an unexported `sync.RWMutex` in Go for the reason `Post`'s is —
//! the struct is shared between goroutines by pointer. `&mut self` is the same guarantee enforced
//! by the compiler, so there is no mutex here and [`Draft::get_props`]/[`Draft::set_props`] exist
//! only because Go's call sites use them.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::post::{POST_FILEIDS_MAX_RUNES, POST_PROPS_MAX_RUNES};
use crate::post_metadata::PostMetadata;
use crate::utils::{
    AppError, AppResult, StringArray, StringInterface, array_to_json, get_millis, is_valid_id,
    remove_duplicate_strings, string_interface_to_json,
};

/// `omitempty` on a map or a slice drops nil **and** empty, and the two are indistinguishable on
/// the wire. The `Option` is kept anyway because [`Draft::pre_commit`] branches on nil.
fn map_is_empty(m: &Option<StringInterface>) -> bool {
    m.as_ref().is_none_or(StringInterface::is_empty)
}

fn slice_is_empty(v: &Option<StringArray>) -> bool {
    v.as_ref().is_none_or(Vec::is_empty)
}

/// Port of `model.Draft` (draft.go:12).
///
/// `#[serde(default)]` for [D-043]: Go leaves an absent key at its zero value, and every client
/// that saves a draft sends a partial object.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Draft {
    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    /// Deprecated in Go — "we now just hard delete the rows". Never validated at any value, and
    /// unconditionally zeroed by [`Self::pre_save`].
    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "channel_id")]
    pub channel_id: String,

    #[serde(rename = "root_id")]
    pub root_id: String,

    #[serde(rename = "message")]
    pub message: String,

    /// Never validated. See the module docs.
    #[serde(rename = "type")]
    pub draft_type: String,

    /// Go marks the field `Deprecated: use GetProps()` because of the mutex; see the module docs
    /// for why there is no mutex here. **No `omitempty`**, so a nil props is `null` on the wire.
    #[serde(rename = "props")]
    pub props: Option<StringInterface>,

    #[serde(rename = "file_ids", skip_serializing_if = "slice_is_empty")]
    pub file_ids: Option<StringArray>,

    #[serde(rename = "metadata", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<PostMetadata>,

    /// Go's type here is `StringInterface`, **not** `*PostPriority`. Untyped and unvalidated.
    #[serde(rename = "priority", skip_serializing_if = "map_is_empty")]
    pub priority: Option<StringInterface>,
}

fn error(
    field: &str,
    params: Option<HashMap<String, serde_json::Value>>,
    details: String,
) -> Box<AppError> {
    Box::new(AppError::new(
        // Plural, and identical in `BaseIsValid`. Go says `Drafts.IsValid` in both.
        "Drafts.IsValid",
        format!("model.draft.is_valid.{field}.app_error"),
        params,
        details,
        400,
    ))
}

impl Draft {
    /// Port of `(*Draft).IsValid` (draft.go:30).
    ///
    /// `max_draft_size` comes from the server config, not from a constant, and is Go's `int` —
    /// **signed**, so it is `i64` here rather than the `usize` [`crate::post::Post::is_valid`]
    /// takes. A negative limit rejects even the empty message (`0 > -1`), which the oracle
    /// records; `usize` could not express the case. See [D-059].
    ///
    /// The message check runs **before** everything in [`Self::base_is_valid`].
    pub fn is_valid(&self, max_draft_size: i64) -> AppResult {
        let runes = self.message.chars().count();
        if runes as i64 > max_draft_size {
            let params = HashMap::from([
                ("Length".to_string(), serde_json::json!(runes)),
                ("MaxLength".to_string(), serde_json::json!(max_draft_size)),
            ]);
            return Err(error("message_length", Some(params), self.channel_detail()));
        }

        self.base_is_valid()
    }

    /// Port of `(*Draft).BaseIsValid` (draft.go:39).
    ///
    /// Exported in Go and called directly by the store, so it is a public entry point here too
    /// rather than a private half of [`Self::is_valid`] — its answers differ, because it skips
    /// the message check entirely.
    pub fn base_is_valid(&self) -> AppResult {
        // Only `== 0` is rejected. A negative timestamp passes both checks.
        if self.create_at == 0 {
            return Err(error("create_at", None, self.channel_detail()));
        }

        if self.update_at == 0 {
            return Err(error("update_at", None, self.channel_detail()));
        }

        // The three id branches carry no detail at all, unlike every other branch here.
        if !is_valid_id(&self.user_id) {
            return Err(error("user_id", None, String::new()));
        }

        if !is_valid_id(&self.channel_id) {
            return Err(error("channel_id", None, String::new()));
        }

        if !(is_valid_id(&self.root_id) || self.root_id.is_empty()) {
            return Err(error("root_id", None, String::new()));
        }

        // Measured over Go's JSON, so a nil list costs four runes (`null`) and not two (`[]`).
        if array_to_json(self.file_ids.as_deref()).chars().count() > POST_FILEIDS_MAX_RUNES {
            return Err(error("file_ids", None, self.channel_detail()));
        }

        // Go's `encoding/json` escaping is part of the measurement: one `<` costs six runes.
        if string_interface_to_json(self.get_props()).chars().count() > POST_PROPS_MAX_RUNES {
            return Err(error("props", None, self.channel_detail()));
        }

        // The same cap, applied a second time to a different field.
        if string_interface_to_json(self.priority.as_ref())
            .chars()
            .count()
            > POST_PROPS_MAX_RUNES
        {
            return Err(error("priority", None, self.channel_detail()));
        }

        Ok(())
    }

    /// Every detail but the three id branches is this. `Post` uses `id=` throughout.
    fn channel_detail(&self) -> String {
        format!("channelid={}", self.channel_id)
    }

    /// Port of `(*Draft).SetProps` (draft.go:75).
    ///
    /// Stores the argument as-is, `None` included — so it can put a draft back into the state
    /// [`Self::pre_commit`] exists to leave.
    pub fn set_props(&mut self, props: Option<StringInterface>) {
        self.props = props;
    }

    /// Port of `(*Draft).GetProps` (draft.go:81).
    pub fn get_props(&self) -> Option<&StringInterface> {
        self.props.as_ref()
    }

    /// Port of `(*Draft).PreSave` (draft.go:87).
    ///
    /// Preserves a non-zero `create_at` (like `Post` and `User`, unlike `Team` and `Session`) and
    /// bumps `update_at` to the clock either way — so `update_at == create_at` only on a first
    /// save. `delete_at` is zeroed unconditionally, discarding whatever a client sent.
    pub fn pre_save(&mut self) {
        if self.create_at == 0 {
            self.create_at = get_millis();
            self.update_at = self.create_at;
        } else {
            self.update_at = get_millis();
        }

        self.delete_at = 0;
        self.pre_commit();
    }

    /// Port of `(*Draft).PreCommit` (draft.go:99).
    ///
    /// Materialises `props` and `file_ids` — after this the wire form carries `"props":{}` rather
    /// than `"props":null`, though `file_ids` stays absent either way. `priority` and `metadata`
    /// are **not** materialised.
    pub fn pre_commit(&mut self) {
        if self.props.is_none() {
            self.set_props(Some(StringInterface::new()));
        }

        // Go's comment: "There's a rare bug where the client sends up duplicate FileIds".
        // `RemoveDuplicateStrings` **sorts** as well as de-duplicating, so the client's order is
        // discarded — and it is byte order, so `"A"` precedes `"a"`.
        let file_ids = self.file_ids.get_or_insert_with(Vec::new);
        remove_duplicate_strings(file_ids);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Draft {
        Draft {
            create_at: 1700000000000,
            update_at: 1700000001000,
            user_id: "6bdz674pgq767e4jx75w4pf57a".to_string(),
            channel_id: "qr6kf7ztp7yifxt4wm5xn51bke".to_string(),
            ..Draft::default()
        }
    }

    #[test]
    fn the_zero_value_carries_props_as_null() {
        // props has no omitempty; file_ids, metadata and priority do.
        assert_eq!(
            serde_json::to_string(&Draft::default()).unwrap(),
            r#"{"create_at":0,"update_at":0,"delete_at":0,"user_id":"","channel_id":"","root_id":"","message":"","type":"","props":null}"#
        );
    }

    #[test]
    fn an_empty_metadata_survives_but_an_empty_priority_does_not() {
        let d = Draft {
            metadata: Some(PostMetadata::default()),
            priority: Some(StringInterface::new()),
            file_ids: Some(Vec::new()),
            ..Draft::default()
        };
        let out = serde_json::to_string(&d).unwrap();
        assert!(out.contains(r#""metadata":{}"#), "{out}");
        assert!(!out.contains("priority"), "{out}");
        assert!(!out.contains("file_ids"), "{out}");
    }

    #[test]
    fn the_message_check_precedes_the_base_checks() {
        let mut d = valid();
        d.message = "abc".to_string();
        d.create_at = 0;

        // Both are broken; the message wins because it is tested first.
        assert_eq!(
            d.is_valid(2).unwrap_err().id,
            "model.draft.is_valid.message_length.app_error"
        );
        assert_eq!(
            d.base_is_valid().unwrap_err().id,
            "model.draft.is_valid.create_at.app_error"
        );
    }

    #[test]
    fn pre_save_zeroes_delete_at_and_keeps_a_set_create_at() {
        let mut d = valid();
        d.delete_at = 1700000005000;
        d.pre_save();
        assert_eq!(d.create_at, 1700000000000);
        assert_eq!(d.delete_at, 0);
        assert!(d.update_at >= 1700000001000);

        let mut fresh = Draft::default();
        fresh.pre_save();
        assert_ne!(fresh.create_at, 0);
        assert_eq!(fresh.update_at, fresh.create_at);
    }

    #[test]
    fn pre_commit_sorts_and_dedups_file_ids() {
        let mut d = Draft {
            file_ids: Some(vec!["b".into(), "A".into(), "b".into(), "a".into()]),
            ..Draft::default()
        };
        d.pre_commit();
        assert_eq!(d.file_ids.as_deref().unwrap(), ["A", "a", "b"]);
        assert_eq!(d.props, Some(StringInterface::new()));
        // Neither of these is materialised.
        assert!(d.priority.is_none() && d.metadata.is_none());
    }
}

/// Parity tests driven by `fixtures/behaviour_draft.json` — Go's own answers.
#[cfg(test)]
mod go_parity {
    use super::*;
    use crate::utils::go_json_marshal;
    use serde_json::Value;

    fn oracle() -> Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_draft.json")).unwrap()
    }

    /// The one document Go accepts and we reject: `null` into a scalar. See [D-057].
    const NULL_SCALAR_ONLY: &str = "null_scalars";

    /// Rebuilds a case's draft, substituting the padding the fixture describes rather than
    /// embeds. See `draftPad` in the generator for why it is not embedded.
    fn draft_of(case: &Value) -> Draft {
        let mut doc = case["draft"].clone();
        if let Some(pad) = case.get("pad").filter(|p| !p.is_null()) {
            let field = pad["field"].as_str().unwrap();
            let key = pad["key"].as_str().unwrap();
            let count = usize::try_from(pad["count"].as_u64().unwrap()).unwrap();
            let value = format!(
                "{}{}",
                pad["prefix"].as_str().unwrap(),
                pad["fill"].as_str().unwrap().repeat(count)
            );
            doc[field][key] = Value::String(value);
        }
        serde_json::from_value(doc).unwrap()
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

            let decoded: Draft = match case["in"].as_str().unwrap() {
                // The zero-value row has no input document; it pins the marshal side alone.
                "" => Draft::default(),
                doc => serde_json::from_str(doc).unwrap_or_else(|e| panic!("{name}: {e}")),
            };

            // Byte-for-byte, which pins the field order and Go's HTML escaping.
            assert_eq!(
                go_json_marshal(&decoded).unwrap(),
                case["out"].as_str().unwrap(),
                "{name}"
            );

            // The JSON cannot distinguish nil from empty for three of the four reference
            // fields, and `pre_commit` branches on exactly that.
            assert_eq!(
                decoded.props.is_none(),
                case["props_nil"].as_bool().unwrap(),
                "{name}: props nil-ness"
            );
            assert_eq!(
                decoded.file_ids.is_none(),
                case["file_ids_nil"].as_bool().unwrap(),
                "{name}: file_ids nil-ness"
            );
            assert_eq!(
                decoded.priority.is_none(),
                case["priority_nil"].as_bool().unwrap(),
                "{name}: priority nil-ness"
            );
            assert_eq!(
                decoded.metadata.is_none(),
                case["metadata_nil"].as_bool().unwrap(),
                "{name}: metadata nil-ness"
            );
            checked += 1;
        }
        assert_eq!(checked, cases.len() - 1, "every case but the null one");
    }

    /// [D-057]: Go leaves a scalar untouched on `null`, serde rejects the document.
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
            go_json_marshal(&Draft::default()).unwrap(),
            "Go decoded it to the zero value"
        );
        assert!(
            serde_json::from_str::<Draft>(doc).is_err(),
            "{doc}: we accepted it, so [D-057] can be closed"
        );
    }

    #[test]
    fn is_valid_matches_go() {
        let oracle = oracle();
        let cases = oracle["is_valid"].as_array().unwrap();
        assert!(cases.len() > 40, "corpus shrank: {}", cases.len());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let draft = draft_of(case);
            let max = case["max_draft_size"].as_i64().unwrap();

            match draft.is_valid(max) {
                Ok(()) => assert_eq!(case["error_id"].as_str().unwrap(), "", "{name}: Go errored"),
                Err(err) => {
                    assert_eq!(err.id, case["error_id"].as_str().unwrap(), "{name}: id");
                    assert_eq!(
                        err.detailed_error,
                        case["detailed"].as_str().unwrap(),
                        "{name}: detail"
                    );
                    assert_eq!(err.where_, case["where"].as_str().unwrap(), "{name}: where");
                    assert_eq!(
                        i64::from(err.status_code),
                        case["status"].as_i64().unwrap(),
                        "{name}: status"
                    );
                }
            }
        }
    }

    #[test]
    fn base_is_valid_matches_go() {
        let oracle = oracle();
        for case in oracle["is_valid"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let draft = draft_of(case);

            match draft.base_is_valid() {
                Ok(()) => assert_eq!(
                    case["base_error_id"].as_str().unwrap(),
                    "",
                    "{name}: Go errored"
                ),
                Err(err) => {
                    assert_eq!(
                        err.id,
                        case["base_error_id"].as_str().unwrap(),
                        "{name}: id"
                    );
                    assert_eq!(
                        err.detailed_error,
                        case["base_detailed"].as_str().unwrap(),
                        "{name}: detail"
                    );
                    assert_eq!(
                        err.where_,
                        case["base_where"].as_str().unwrap(),
                        "{name}: where"
                    );
                }
            }
        }
    }

    /// The message check runs before `base_is_valid`, so at least one case must report a
    /// `message_length` from `IsValid` while `BaseIsValid` reports something else entirely.
    /// Without this the ordering could regress and every individual assertion above would still
    /// pass — each is checked against its own function.
    #[test]
    fn the_corpus_proves_the_check_order() {
        let oracle = oracle();
        let ordering = oracle["is_valid"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| {
                c["error_id"].as_str() == Some("model.draft.is_valid.message_length.app_error")
                    && !c["base_error_id"].as_str().unwrap().is_empty()
            })
            .count();
        assert!(ordering >= 2, "corpus lost its ordering cases");
    }

    #[test]
    fn pre_save_matches_go() {
        let oracle = oracle();
        let cases = oracle["pre_save"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut draft: Draft = serde_json::from_value(case["in"].clone()).unwrap();
            let in_create = draft.create_at;
            let in_update = draft.update_at;
            draft.pre_save();

            let kept = draft.create_at == in_create && in_create != 0;
            assert_eq!(
                kept,
                case["create_at_was_kept"].as_bool().unwrap(),
                "{name}: create_at kept"
            );
            if kept {
                assert_eq!(
                    draft.create_at,
                    case["create_at_value"].as_i64().unwrap(),
                    "{name}: create_at value"
                );
            }
            assert_eq!(
                draft.update_at == draft.create_at,
                case["update_at_equals_create_at"].as_bool().unwrap(),
                "{name}: update_at == create_at"
            );
            assert_eq!(
                draft.update_at != in_update,
                case["update_at_moved"].as_bool().unwrap(),
                "{name}: update_at moved"
            );
            assert_collections_match(&draft, case, name);
        }
    }

    #[test]
    fn pre_commit_matches_go() {
        let oracle = oracle();
        let cases = oracle["pre_commit"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut draft: Draft = serde_json::from_value(case["in"].clone()).unwrap();
            let (in_create, in_update) = (draft.create_at, draft.update_at);
            draft.pre_commit();

            // PreCommit touches no timestamp at all — including delete_at, which PreSave zeroes.
            assert_eq!(draft.create_at, in_create, "{name}: create_at moved");
            assert_eq!(draft.update_at, in_update, "{name}: update_at moved");
            assert_eq!(
                draft.create_at,
                case["create_at_value"].as_i64().unwrap(),
                "{name}: create_at"
            );
            assert_collections_match(&draft, case, name);
        }
    }

    /// The half of a pre-hook case that is identical for both hooks.
    fn assert_collections_match(draft: &Draft, case: &Value, name: &str) {
        assert_eq!(
            draft.delete_at,
            case["delete_at_out"].as_i64().unwrap(),
            "{name}: delete_at"
        );
        assert_eq!(
            draft.props.is_none(),
            case["props_nil_out"].as_bool().unwrap(),
            "{name}: props nil"
        );
        assert_eq!(
            serde_json::to_value(&draft.props).unwrap(),
            case["props_out"],
            "{name}: props"
        );
        assert_eq!(
            draft.file_ids.is_none(),
            case["file_ids_nil_out"].as_bool().unwrap(),
            "{name}: file_ids nil"
        );
        assert_eq!(
            serde_json::to_value(&draft.file_ids).unwrap(),
            case["file_ids_out"],
            "{name}: file_ids"
        );
        assert_eq!(
            draft.priority.is_none(),
            case["priority_nil_out"].as_bool().unwrap(),
            "{name}: priority nil"
        );
    }

    #[test]
    fn the_props_accessors_match_go() {
        let oracle = oracle();
        let cases = oracle["props_accessors"].as_array().unwrap();
        assert_eq!(cases.len(), 3);

        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut draft = Draft {
                props: Some(StringInterface::from_iter([(
                    "pre".to_string(),
                    Value::String("existing".to_string()),
                )])),
                ..Draft::default()
            };
            draft.set_props(match name {
                "set_nil" => None,
                "set_empty" => Some(StringInterface::new()),
                "set_value" => Some(StringInterface::from_iter([(
                    "a".to_string(),
                    Value::String("b".to_string()),
                )])),
                other => panic!("unknown case {other}"),
            });

            assert_eq!(
                draft.get_props().is_none(),
                case["nil_out"].as_bool().unwrap(),
                "{name}: nil-ness"
            );
            assert_eq!(
                go_json_marshal(&draft.get_props()).unwrap(),
                case["json_out"].as_str().unwrap(),
                "{name}: props"
            );
            assert_eq!(
                go_json_marshal(&draft).unwrap(),
                case["draft"].as_str().unwrap(),
                "{name}: draft"
            );
        }
    }
}

/// Serialization parity against `fixtures/draft.json` — every field non-zero.
#[cfg(test)]
mod fixture {
    use super::*;

    #[test]
    fn round_trips_the_generated_fixture() {
        let raw = include_str!("../../../fixtures/draft.json");
        let decoded: Draft = serde_json::from_str(raw).unwrap();

        // Guard against a zero-valued fixture: the four omitempty fields would vanish and the
        // round trip would prove nothing about precisely the fields most likely to drift.
        assert!(!decoded.user_id.is_empty() && !decoded.message.is_empty());
        assert!(decoded.create_at != 0 && decoded.update_at != 0 && decoded.delete_at != 0);
        assert!(!decoded.file_ids.as_deref().unwrap_or_default().is_empty());
        assert!(!decoded.props.as_ref().unwrap().is_empty());
        assert!(!decoded.priority.as_ref().unwrap().is_empty());
        assert!(decoded.metadata.is_some());

        let ours: serde_json::Value = serde_json::to_value(&decoded).unwrap();
        let theirs: serde_json::Value = serde_json::from_str(raw).unwrap();
        assert_eq!(ours, theirs);
    }
}
